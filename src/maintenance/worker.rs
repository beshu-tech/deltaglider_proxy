// SPDX-License-Identifier: BUSL-1.1

//! The maintenance runner: a single background task that claims queued
//! jobs (oldest first, one at a time — bounded resource usage) and
//! executes the canned re-encryption procedure:
//!
//! 1. **drain** — wait for the gated bucket's in-flight S3 writes to
//!    reach zero (the gate rejects NEW writes, but one admitted moments
//!    before it armed could otherwise land mid-rewrite and be lost).
//! 2. **counting** — one LIST sweep for the exact object total. The
//!    write set is frozen by the gate, so the total cannot drift and the
//!    progress bar is honest.
//! 3. **objects** — per object: `engine.head` → [`needs_rewrite`]? then
//!    rewrite in place via `transfer::copy_object_with_retries`
//!    (source == destination; the engine's store path encrypts/decrypts
//!    per the CURRENT backend mode; stale markers stripped). Failures are
//!    recorded per object and the job continues.
//! 4. **references** — deltaspace `reference.bin` blobs are shared
//!    storage-level artifacts the object sweep never rewrites; re-store
//!    them through the (encrypting) storage wrapper when their state
//!    doesn't match.
//!
//! Progress + the continuation token are persisted after every page, so
//! a crash/restart resumes mid-bucket (boot reconcile re-queues the job
//! with its cursor intact). Cancellation is checked per page.
//!
//! [`needs_rewrite`]: super::needs_rewrite

use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::api::handlers::AppState;
use crate::background::LeaseKeeper;
use crate::config::SharedConfig;
use crate::config_apply::ConfigMutator;
use crate::config_db::ConfigDb;
use crate::config_sections::LeaseTiming;
use crate::coordination::LeaseError;
use crate::storage::encrypting::{ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY};
use crate::transfer::{copy_object_with_retries, ObjectTransferRequest};

use super::paged::{paged_phase, JobCtx, KeyPage, PageStep, PhaseSpec};
use super::store::{current_unix_seconds, MaintenanceJob};
use super::{needs_rewrite, resolve_desired, strip_encryption_markers, DesiredEncryption};

const POLL_INTERVAL_SECS: u64 = 3;
pub(crate) const PAGE_SIZE: u32 = 1000;
const MAX_FAILURES_RETAINED: usize = 200;
const DRAIN_POLL_MS: u64 = 250;

/// Who holds a job's lease, and its timing (`advanced.jobs`, read at
/// claim). The claim, the keeper and the per-page heartbeat all renew
/// with the same TTL.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Holder<'a> {
    pub id: &'a str,
    pub lease: LeaseTiming,
}

#[cfg(test)]
impl<'a> Holder<'a> {
    pub(crate) fn test(id: &'a str) -> Self {
        Self {
            id,
            lease: LeaseTiming::MAINTENANCE,
        }
    }
}

/// Spawn the maintenance worker loop. Wakes on `state.maintenance_notify`
/// (job creation) or every few seconds (boot-requeued jobs, lease retry).
pub fn spawn_worker(
    mutator: ConfigMutator,
    db: Arc<Mutex<ConfigDb>>,
) -> tokio::task::JoinHandle<()> {
    let config: SharedConfig = mutator.config.clone();
    let state: Arc<AppState> = mutator.app.clone();
    let instance_id = format!("maintenance:{}", uuid::Uuid::new_v4());
    tokio::spawn(async move {
        info!("Maintenance worker started: instance_id={}", instance_id);
        loop {
            tokio::select! {
                _ = state.maintenance_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)) => {}
                _ = crate::shutdown::started() => {}
            }
            // Drain every claimable job before sleeping again.
            loop {
                if crate::shutdown::is_shutting_down() {
                    info!("Maintenance worker stopped: the process shuts down");
                    return;
                }
                let lease = config
                    .read()
                    .await
                    .jobs
                    .lease_timing(LeaseTiming::MAINTENANCE);
                let claimed = {
                    let db = db.lock().await;
                    // Re-queue abandoned rows first (lease-aware): a job
                    // whose runner died mid-run becomes claimable within
                    // one lease TTL instead of waiting for the next boot.
                    if let Err(e) = db.maintenance_requeue_abandoned() {
                        warn!("maintenance: requeue scan failed: {}", e);
                    }
                    let claimed = db.maintenance_claim_next_job(
                        &instance_id,
                        current_unix_seconds(),
                        lease.ttl_secs,
                    );
                    // Every tick re-derives the gate: the claim (a migrate
                    // resumed in `cleanup` gates nothing), the requeue, and
                    // any transition a crashed path did not sync.
                    state.maintenance_gate.sync_from(&db);
                    claimed
                };
                match claimed {
                    Ok(Some(job)) => {
                        let holder = Holder {
                            id: &instance_id,
                            lease,
                        };
                        run_job(&mutator, &config, &db, &state, holder, job).await;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!("maintenance: claim failed: {}", e);
                        break;
                    }
                }
            }
        }
    })
}

/// Execute one claimed job to a terminal state. Never panics the loop:
/// every failure path settles the row and releases the gate.
async fn run_job(
    mutator: &ConfigMutator,
    config: &SharedConfig,
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: MaintenanceJob,
) {
    let bucket = job.bucket.clone();
    // The write gate needs nothing here: the claim above synced it from the
    // rows (armed since creation; a migrate resumed in `cleanup` is not).

    info!(
        "maintenance: job #{} ({}) starting on bucket '{}' (phase={}, resuming={})",
        job.id,
        job.kind,
        bucket,
        job.phase,
        job.continuation_token.is_some()
    );
    crate::audit::audit_log(
        &format!("maintenance_{}_start", job.kind),
        job.triggered_by.as_deref().unwrap_or("system"),
        &format!("job:{}", job.id),
        &axum::http::HeaderMap::new(),
        &bucket,
        "",
    );

    let keeper = spawn_lease_keeper(
        db.clone(),
        job.id,
        holder.id.to_string(),
        holder.lease.ttl_secs,
        std::time::Duration::from_secs(holder.lease.heartbeat_secs.max(1) as u64),
    );
    let outcome = match job.kind.as_str() {
        "reencrypt" => execute_phases(config, db, state, holder, &job).await,
        "migrate" => super::migrate::execute_migrate_phases(mutator, db, state, holder, &job).await,
        super::backfill::KIND => {
            super::backfill::execute_backfill_phases(db, state, holder, &job).await
        }
        other => Err(format!("unknown maintenance job kind '{other}'").into()),
    };
    drop(keeper);

    let action = after_run(&outcome, crate::shutdown::is_shutting_down());
    if action == AfterRun::ReleaseForResume {
        // A graceful shutdown (SIGTERM on every rolling deploy) stopped the
        // job, or made its work fail. Neither says anything about the data:
        // never settle. Hand the row back with its cursor, so the next boot
        // resumes it at once instead of after one lease TTL.
        // The row goes back to `queued`, still active, so its gate stays
        // armed until the next run settles it.
        let released = {
            let db = db.lock().await;
            let released = db.maintenance_release_for_resume(job.id, holder.id);
            state.maintenance_gate.sync_from(&db);
            released
        };
        warn!(
            "maintenance: job #{} on '{}' stopped by shutdown ({}) — left resumable \
             (released: {:?})",
            job.id,
            bucket,
            outcome
                .as_ref()
                .err()
                .map(PhaseStop::to_string)
                .unwrap_or_default(),
            released
        );
        return;
    }

    if action == AfterRun::LeaveToLeaseHolder {
        // Do NOT settle the row: losing the lease means it lapsed (the
        // requeue scan will hand it to the next claimer with its cursor
        // intact) or another instance already claimed it — settling here
        // would terminate THAT run's row out from under it. Just stop; the
        // gate stays armed for as long as the row is active.
        warn!(
            "maintenance: job #{} on '{}' lost its lease — stopping without settling \
             (the job resumes under the next claimer)",
            job.id, bucket
        );
        state.maintenance_gate.sync_from(&*db.lock().await);
        return;
    }

    let (status, last_error) = match &outcome {
        Ok(()) => {
            // Per-object failures do not stop a job; they decide its status.
            let row = db.lock().await.maintenance_job_by_id(job.id).ok().flatten();
            row.map(|j| super::settle_status(j.objects_done, j.objects_skipped, j.objects_failed))
                .unwrap_or(("completed", None))
        }
        Err(PhaseStop::Cancelled) => ("cancelled", None),
        Err(e) => ("failed", Some(e.to_string())),
    };
    // A phase can pre-settle its row (migrate cleanup: a note and
    // `completed_with_errors`); `maintenance_finish` then leaves it alone,
    // and the log + audit must report the row's status, not the recomputed one.
    let status = {
        let db = db.lock().await;
        if let Err(e) = db.maintenance_finish(job.id, status, last_error.as_deref()) {
            warn!("maintenance: failed to settle job #{}: {}", job.id, e);
        }
        state.maintenance_gate.sync_from(&db);
        let row = db.maintenance_job_by_id(job.id).ok().flatten();
        settled_status(status, row.as_ref().map(|j| j.status.as_str())).to_string()
    };
    info!(
        "maintenance: job #{} on '{}' finished: {}{}",
        job.id,
        bucket,
        status,
        last_error
            .as_deref()
            .map(|e| format!(" ({e})"))
            .unwrap_or_default()
    );
    crate::audit::audit_log(
        &format!("maintenance_{}_{status}", job.kind),
        job.triggered_by.as_deref().unwrap_or("system"),
        &format!("job:{}", job.id),
        &axum::http::HeaderMap::new(),
        &bucket,
        "",
    );
}

/// The three phases, resumable at page granularity. Returns Err(reason)
/// only for job-fatal conditions (per-object failures are recorded and
/// skipped over instead).
async fn execute_phases(
    config: &SharedConfig,
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
) -> Result<(), PhaseStop> {
    let bucket = &job.bucket;

    // ── Drain in-flight writes admitted before the gate armed. ──
    drain_inflight_writes(state, bucket).await?;

    let mut visitor = Reencrypt {
        config,
        bucket,
        desired: None,
    };
    let mut c = run_count_then_walk(db, state, holder, job, "rewrite", &mut visitor).await?;
    let mut phase = job.phase.clone();
    if phase == "counting" || phase == "objects" {
        phase = "references".to_string();
        persist(db, job, &phase, &c, None).await;
    }

    // ── Phase: references (deltaspace reference.bin blobs) ──
    if phase == "references" {
        check_cancel(db, job.id).await?;
        let desired = {
            let cfg = config.read().await;
            resolve_desired(&cfg, bucket).map_err(|e| format!("config changed mid-run: {e}"))?
        };
        let engine = state.engine.load().clone();
        let deltaspaces = engine
            .storage()
            .list_deltaspaces(bucket)
            .await
            .map_err(|e| format!("list deltaspaces failed: {e}"))?;
        for prefix in deltaspaces {
            // A graceful stop waits for the end of the current object only.
            stop_if_shutting_down()?;
            check_cancel(db, job.id).await?;
            match rewrite_reference_if_needed(&engine, bucket, &prefix, &desired).await {
                Ok(()) => {}
                Err(e) => {
                    record_failure(db, job.id, &format!("{prefix}/.dg/reference.bin"), &e).await?;
                    c.failed += 1;
                }
            }
            heartbeat(db, job.id, holder).await?;
        }
        // Final counters (the per-reference failure increments above).
        persist(db, job, "references", &c, None).await;
    }

    Ok(())
}

/// The reencrypt per-object step: rewrite an object whose at-rest state
/// does not match the backend's current mode.
struct Reencrypt<'a> {
    config: &'a SharedConfig,
    bucket: &'a str,
    desired: Option<DesiredEncryption>,
}

impl ObjectVisitor for Reencrypt<'_> {
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        // Re-resolve the desired state every page: a config apply mid-run
        // swaps the engine; the job must follow (or abort if the mode
        // became unsupported).
        let cfg = self.config.read().await;
        self.desired = Some(
            resolve_desired(&cfg, self.bucket)
                .map_err(|e| format!("config changed mid-run: {e}"))?,
        );
        Ok(())
    }

    async fn visit(
        &mut self,
        engine: &Arc<crate::deltaglider::DynEngine>,
        key: &str,
        meta: &crate::types::FileMetadata,
    ) -> Result<Visit, PhaseStop> {
        let desired = self.desired.as_ref().expect("begin_page runs first");
        if !needs_rewrite(&meta.user_metadata, desired) {
            return Ok(Visit::Skipped);
        }
        let req = ObjectTransferRequest {
            source_bucket: self.bucket,
            source_key: key,
            destination_bucket: self.bucket,
            destination_key: key,
            provenance: None,
            // Shed stale markers; the encrypting wrapper re-stamps
            // fresh ones when the destination mode encrypts.
            strip_user_metadata_keys: &[ENCRYPTION_MARKER_KEY, ENCRYPTION_KEY_ID_KEY],
            operation: "maintenance-reencrypt",
            upload_concurrency: None,
            keep_created_at: true,
        };
        Ok(match copy_object_with_retries(engine, req).await {
            Ok(outcome) => Visit::Done {
                bytes: outcome.bytes_copied as i64,
            },
            Err(e) => Visit::Failed(e.to_string()),
        })
    }
}

/// What one object of an `objects` phase ended in.
pub(crate) enum Visit {
    Done {
        bytes: i64,
    },
    Skipped,
    /// Recorded in the failure ring and counted; the phase goes on.
    Failed(String),
}

/// The per-object work of a count-then-walk job kind (reencrypt,
/// backfill-metadata). [`run_count_then_walk`] owns the rest.
pub(crate) trait ObjectVisitor {
    /// Runs before each listing page.
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// One user object, with the metadata the driver read for it.
    async fn visit(
        &mut self,
        engine: &Arc<crate::deltaglider::DynEngine>,
        key: &str,
        meta: &crate::types::FileMetadata,
    ) -> Result<Visit, PhaseStop>;
}

/// A job's counters, as its row stores them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    pub total: Option<i64>,
    pub done: i64,
    pub skipped: i64,
    pub failed: i64,
    pub bytes: i64,
}

impl Counters {
    /// The counters the row holds now.
    pub(crate) fn of(job: &MaintenanceJob) -> Self {
        Self {
            total: job.objects_total,
            done: job.objects_done,
            skipped: job.objects_skipped,
            failed: job.objects_failed,
            bytes: job.bytes_done,
        }
    }
}

/// The `counting` → `objects` phases that the reencrypt and backfill kinds
/// share, on [`paged_phase`]: per-object failures recorded and skipped
/// over. Returns the counters at the end of `objects`; a job persisted in
/// a later phase gets its row's counters back untouched. `what` names the
/// job in the page-budget error.
pub(crate) async fn run_count_then_walk<V: ObjectVisitor>(
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
    what: &str,
    visitor: &mut V,
) -> Result<Counters, PhaseStop> {
    let mut phase = job.phase.clone();
    let mut c = Counters::of(job);

    // ── Phase: counting ──
    if phase == "counting" {
        let count = counting_phase(db, state, holder, job, job.continuation_token.clone()).await?;
        c = Counters {
            total: Some(count),
            ..Counters::default()
        };
        phase = "objects".to_string();
        persist(db, job, &phase, &c, None).await;
    }

    // ── Phase: objects ──
    if phase != "objects" {
        return Ok(c);
    }
    let budget = format!(
        "{what} stopped at the page budget with more pages pending — bucket \
         too large for one pass; job left resumable in phase 'objects' \
         (cursor persisted)"
    );
    let mut step = ObjectsStep {
        db,
        state,
        job,
        visitor,
        engine: None,
        c,
    };
    paged_phase(
        JobCtx { db, holder, job },
        PhaseSpec {
            phase: "objects",
            // Resume only when the job was persisted IN this phase (a fresh
            // transition from counting starts at page 0).
            resume: resumable_in(job, "objects"),
            cancel_every: None,
            budget_exhausted: &budget,
        },
        &mut step,
    )
    .await?;
    Ok(step.c)
}

/// The saved cursor of a job persisted IN `phase` (else a fresh start).
pub(crate) fn resumable_in(job: &MaintenanceJob, phase: &str) -> Option<String> {
    if job.phase == phase {
        job.continuation_token.clone()
    } else {
        None
    }
}

/// The `objects` phase: HEAD each object, hand it to the visitor.
struct ObjectsStep<'a, V> {
    db: &'a Arc<Mutex<ConfigDb>>,
    state: &'a Arc<AppState>,
    job: &'a MaintenanceJob,
    visitor: &'a mut V,
    /// The page's engine (one per page: an apply mid-page swaps it).
    engine: Option<Arc<crate::deltaglider::DynEngine>>,
    c: Counters,
}

impl<V: ObjectVisitor> PageStep for ObjectsStep<'_, V> {
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        self.visitor.begin_page().await?;
        self.engine = Some(self.state.engine.load().clone());
        Ok(())
    }

    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, String> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        engine
            .list_objects(&self.job.bucket, "", None, PAGE_SIZE, token, false)
            .await
            .map(KeyPage::from)
            .map_err(|e| format!("object list failed: {e}"))
    }

    // A restart re-scans from page 0: the visitor skips objects already
    // done, so the re-scan is idempotent. Counters are NOT reset:
    // re-encountered objects tally as `skipped` again — display drift only,
    // never a second rewrite.

    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
        persist(self.db, self.job, "objects", &self.c, token).await;
        Ok(())
    }

    async fn object(&mut self, key: &str) -> Result<(), PhaseStop> {
        let engine = self.engine.clone().expect("begin_page runs first");
        let visit = match engine.head(&self.job.bucket, key).await {
            Ok(meta) => self.visitor.visit(&engine, key, &meta).await?,
            Err(e) => Visit::Failed(format!("could not read object metadata: {e}")),
        };
        match visit {
            Visit::Done { bytes } => {
                self.c.done += 1;
                self.c.bytes += bytes;
            }
            Visit::Skipped => self.c.skipped += 1,
            Visit::Failed(reason) => {
                record_failure(self.db, self.job.id, key, &reason).await?;
                self.c.failed += 1;
            }
        }
        Ok(())
    }
}

/// The shared `counting` phase: one LIST sweep for the exact object total
/// (the write gate freezes the write set, so the total cannot drift and
/// the progress bar is honest). Persists progress + cursor per page; used
/// by the reencrypt AND backfill-metadata kinds.
pub(crate) async fn counting_phase(
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
    resume_token: Option<String>,
) -> Result<i64, PhaseStop> {
    // A resumed count keeps the pages before its cursor: the row's total
    // is the count up to that cursor (persisted with it, page by page).
    let count: i64 = match (&resume_token, job.phase.as_str()) {
        (Some(_), "counting") => job.objects_total.unwrap_or(0),
        _ => 0,
    };
    let mut step = CountStep {
        db,
        state,
        job,
        count,
    };
    paged_phase(
        JobCtx { db, holder, job },
        PhaseSpec {
            phase: "counting",
            resume: resume_token,
            cancel_every: None,
            budget_exhausted: "counting stopped at the page budget with more pages \
                 pending — bucket too large for one pass; job left resumable",
        },
        &mut step,
    )
    .await?;
    Ok(step.count)
}

/// The `counting` phase: count the user objects of each page.
struct CountStep<'a> {
    db: &'a Arc<Mutex<ConfigDb>>,
    state: &'a Arc<AppState>,
    job: &'a MaintenanceJob,
    count: i64,
}

impl PageStep for CountStep<'_> {
    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, String> {
        let engine = self.state.engine.load().clone();
        engine
            .list_objects(&self.job.bucket, "", None, PAGE_SIZE, token, false)
            .await
            .map(KeyPage::from)
            .map_err(|e| format!("counting list failed: {e}"))
    }

    /// Recount from page 0 (idempotent).
    fn on_restart(&mut self) {
        self.count = 0;
    }

    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
        let c = Counters {
            total: Some(self.count),
            ..Counters::default()
        };
        persist(self.db, self.job, "counting", &c, token).await;
        Ok(())
    }

    async fn object(&mut self, _key: &str) -> Result<(), PhaseStop> {
        self.count += 1;
        Ok(())
    }
}

/// Re-store a deltaspace's reference blob through the (encrypting)
/// storage wrapper when its at-rest state doesn't match. The reference's
/// PLAINTEXT bytes are unchanged, so the engine's in-memory
/// ReferenceCache (keyed by content) stays valid. The check and the
/// rewrite run under the deltaspace's prefix lock AND the cross-instance
/// reference lock (`with_dest_prefix_lock`), like every reference write.
async fn rewrite_reference_if_needed(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    prefix: &str,
    desired: &DesiredEncryption,
) -> Result<(), String> {
    let res: Result<(), crate::deltaglider::EngineError> = engine
        .with_dest_prefix_lock(bucket, prefix, || async {
            let storage = engine.storage();
            if !storage.has_reference(bucket, prefix).await? {
                return Ok(());
            }
            let meta = storage.get_reference_metadata(bucket, prefix).await?;
            if !needs_rewrite(&meta.user_metadata, desired) {
                return Ok(());
            }
            let data = engine.get_reference_raw(bucket, prefix).await?;
            let mut new_meta = meta;
            strip_encryption_markers(&mut new_meta.user_metadata);
            engine
                .put_reference_raw(bucket, prefix, &data, &new_meta)
                .await?;
            Ok(())
        })
        .await;
    res.map_err(|e| format!("reference rewrite failed for {bucket}/{prefix}: {e}"))
}

/// Wait for the gated bucket's in-flight S3 writes to reach zero. The
/// gate rejects NEW writes from job creation; this waits out the ones
/// admitted before it armed (bounded by the server request timeout — no
/// request legitimately outlives it).
pub(crate) async fn drain_inflight_writes(
    state: &Arc<AppState>,
    bucket: &str,
) -> Result<(), String> {
    let drain_ceiling_secs: u64 =
        crate::config::env_parse_with_default("DGP_REQUEST_TIMEOUT_SECS", 300);
    let drain_deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(drain_ceiling_secs);
    while state.maintenance_gate.inflight_writes(bucket) > 0 {
        if std::time::Instant::now() > drain_deadline {
            return Err(format!(
                "in-flight writes to '{}' did not drain within {}s",
                bucket, drain_ceiling_secs
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(DRAIN_POLL_MS)).await;
    }
    Ok(())
}

/// Cancellation check between pages. Maps "operator asked to cancel"
/// into the Err channel so phases unwind; the caller distinguishes it.
pub(crate) async fn check_cancel(db: &Arc<Mutex<ConfigDb>>, job_id: i64) -> Result<(), PhaseStop> {
    let db = db.lock().await;
    match db.maintenance_cancel_requested(job_id) {
        Ok(true) => Err(PhaseStop::Cancelled),
        _ => Ok(()),
    }
}

/// Why a phase stopped before its end. Only `Failed` carries text: a
/// phase cannot wrap a stop in an error message (`format!` needs text),
/// so a cancel, a lost lease or a shutdown always reaches [`after_run`]
/// as itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PhaseStop {
    /// The operator asked to cancel.
    Cancelled,
    /// This worker's lease was not renewed. The job row is left UNTOUCHED
    /// (no settle, no unwind) — it belongs to whoever holds the lease now,
    /// or to the requeue scan once it lapses.
    LeaseLost,
    /// The process shuts down. Like `LeaseLost`, the row is not settled;
    /// the worker hands it back for the next boot to resume.
    ShuttingDown,
    /// A job-fatal error.
    Failed(String),
}

impl std::fmt::Display for PhaseStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhaseStop::Cancelled => f.write_str("cancelled"),
            PhaseStop::LeaseLost => f.write_str("lease lost"),
            PhaseStop::ShuttingDown => f.write_str("shutting down"),
            PhaseStop::Failed(e) => f.write_str(e),
        }
    }
}

impl From<String> for PhaseStop {
    fn from(e: String) -> Self {
        PhaseStop::Failed(e)
    }
}

impl From<&str> for PhaseStop {
    fn from(e: &str) -> Self {
        PhaseStop::Failed(e.to_string())
    }
}

/// Stop point for phase loops: `Err(ShuttingDown)` once shutdown starts.
pub(crate) fn stop_if_shutting_down() -> Result<(), PhaseStop> {
    if crate::shutdown::is_shutting_down() {
        Err(PhaseStop::ShuttingDown)
    } else {
        Ok(())
    }
}

/// What the worker does with a finished phase run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterRun {
    /// Settle the row (succeeded / completed_with_errors / failed / cancelled).
    Settle,
    /// The lease lapsed or moved: leave the row to whoever holds it now.
    LeaveToLeaseHolder,
    /// Shutdown: never settle, never unwind; hand the row back to resume.
    ReleaseForResume,
}

/// The decision for [`AfterRun`]. Any error while the process shuts down
/// counts as the shutdown: the runtime teardown makes healthy work fail
/// (a refused `spawn_blocking`), and settling on it leaves a half-done
/// bucket marked finished. An operator cancel stays a cancel.
pub(crate) fn after_run(outcome: &Result<(), PhaseStop>, shutting_down: bool) -> AfterRun {
    match outcome {
        Ok(()) => AfterRun::Settle,
        Err(PhaseStop::LeaseLost) => AfterRun::LeaveToLeaseHolder,
        Err(PhaseStop::ShuttingDown) => AfterRun::ReleaseForResume,
        Err(PhaseStop::Cancelled) => AfterRun::Settle,
        Err(PhaseStop::Failed(_)) if shutting_down => AfterRun::ReleaseForResume,
        Err(PhaseStop::Failed(_)) => AfterRun::Settle,
    }
}

/// Save the job's progress. Private: the migrate phases save through
/// [`persist_flushed`], which takes the proof that their deferred copies
/// are durable.
async fn persist(
    db: &Arc<Mutex<ConfigDb>>,
    job: &MaintenanceJob,
    phase: &str,
    c: &Counters,
    token: Option<&str>,
) {
    let db = db.lock().await;
    if let Err(e) = db.maintenance_update_progress(
        job.id, phase, c.total, c.done, c.skipped, c.failed, c.bytes, token,
    ) {
        warn!(
            "maintenance: progress persist failed for job #{}: {}",
            job.id, e
        );
    }
}

/// [`persist`] for a phase that writes without the per-object fsync: only
/// a [`super::migrate::Flushed`] (made by a successful flush) opens it, so
/// a saved cursor never points past a copy that a crash can lose.
pub(crate) async fn persist_flushed(
    _proof: super::migrate::Flushed,
    db: &Arc<Mutex<ConfigDb>>,
    job: &MaintenanceJob,
    phase: &str,
    c: &Counters,
    token: Option<&str>,
) {
    persist(db, job, phase, c, token).await;
}

/// The job's lease keeper. The per-page `heartbeat` is not enough on its
/// own: one page of copy work can outlast the TTL, and a lapsed lease lets
/// the requeue scan hand the job back and the write gate open mid-job.
/// When the keeper gives the lease up, the next per-page `heartbeat`
/// reports `LeaseLost` (a lapsed lease never renews).
pub(crate) fn spawn_lease_keeper(
    db: Arc<Mutex<ConfigDb>>,
    job_id: i64,
    instance_id: String,
    ttl_secs: i64,
    interval: std::time::Duration,
) -> LeaseKeeper {
    LeaseKeeper::spawn(
        format!("maintenance job #{job_id}"),
        interval,
        std::time::Duration::from_secs(ttl_secs.max(1) as u64),
        move || {
            let db = db.clone();
            let instance_id = instance_id.clone();
            async move {
                let db = db.lock().await;
                LeaseError::from_renewal(db.maintenance_heartbeat(
                    job_id,
                    &instance_id,
                    current_unix_seconds(),
                    ttl_secs,
                ))
            }
        },
    )
}

/// Renew the job lease; `Err(LeaseLost)` means the renewal was refused
/// (lapsed, or taken by another instance) and the phase MUST stop — this
/// is the one subsystem that flips config and deletes source data, so a
/// lapsed worker must never keep going. A DB error is NOT a refusal (same
/// verdict as the keeper): stopping on it cleared the write gate while the
/// row stayed active. A lapsed lease never renews, so once the DB answers
/// again a lapse still reads as `LeaseLost`.
pub(crate) async fn heartbeat(
    db: &Arc<Mutex<ConfigDb>>,
    job_id: i64,
    holder: Holder<'_>,
) -> Result<(), PhaseStop> {
    let renewed = {
        let db = db.lock().await;
        db.maintenance_heartbeat(
            job_id,
            holder.id,
            current_unix_seconds(),
            holder.lease.ttl_secs,
        )
    };
    match LeaseError::from_renewal(renewed) {
        Ok(()) => Ok(()),
        Err(LeaseError::Lost) => Err(PhaseStop::LeaseLost),
        Err(LeaseError::Backend(e)) => {
            warn!(
                "maintenance: job #{job_id} per-page lease renewal failed ({e}); \
                 the keeper retries it"
            );
            Ok(())
        }
    }
}

/// Pure: the status to log and audit after the settle. The row's own
/// terminal status wins over the recomputed one (a phase may pre-settle).
pub(crate) fn settled_status<'a>(computed: &'a str, row_status: Option<&'a str>) -> &'a str {
    match row_status {
        // Not one of store.rs's ACTIVE_STATUSES.
        Some(s) if !matches!(s, "queued" | "running" | "cancelling") => s,
        _ => computed,
    }
}

/// Record a per-object failure. Refuses with `Err(ShuttingDown)` once the
/// process shuts down: a failure then is most likely the runtime teardown,
/// not the object, and counting it would settle a healthy job with errors.
/// Callers propagate the Err so the phase stops before its page persists.
pub(crate) async fn record_failure(
    db: &Arc<Mutex<ConfigDb>>,
    job_id: i64,
    key: &str,
    error: &str,
) -> Result<(), PhaseStop> {
    stop_if_shutting_down()?;
    let db = db.lock().await;
    if let Err(e) = db.maintenance_record_failure(job_id, key, error, MAX_FAILURES_RETAINED) {
        warn!(
            "maintenance: failure record failed for job #{}: {}",
            job_id, e
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claimed_job(ttl: i64) -> (Arc<Mutex<ConfigDb>>, i64) {
        let db = ConfigDb::in_memory("testpass").unwrap();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("inst", current_unix_seconds(), ttl)
            .unwrap()
            .expect("claim");
        (Arc::new(Mutex::new(db)), id)
    }

    /// D11: one listing page (1000 objects of copy work) can take longer than
    /// the lease TTL. The per-page heartbeat alone lets the lease lapse, the
    /// requeue scan hands the job back to the queue and the write gate opens
    /// mid-job. A lease keeper renews on a timer for the whole job.
    #[tokio::test]
    async fn lease_keeper_holds_the_lease_through_a_long_page() {
        let ttl = 2;
        // Control: without the keeper, a 3.5s "page" lapses a 2s lease.
        let (db, _) = claimed_job(ttl);
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        assert_eq!(db.lock().await.maintenance_requeue_abandoned().unwrap(), 1);

        let (db, id) = claimed_job(ttl);
        let _keeper = spawn_lease_keeper(
            db.clone(),
            id,
            "inst".to_string(),
            ttl,
            std::time::Duration::from_millis(300),
        );
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        assert_eq!(
            db.lock().await.maintenance_requeue_abandoned().unwrap(),
            0,
            "the lease lapsed while the keeper ran"
        );
    }

    /// A renewal that ERRORS (a busy or briefly unreadable DB) is not a
    /// refusal: the keeper must try again, not stop for good and let the
    /// lease lapse mid-job.
    #[tokio::test]
    async fn lease_keeper_survives_a_transient_db_error() {
        let ttl = 2;
        let (db, id) = claimed_job(ttl);
        let _keeper = spawn_lease_keeper(
            db.clone(),
            id,
            "inst".to_string(),
            ttl,
            std::time::Duration::from_millis(300),
        );
        let sql = |q: &'static str| {
            let db = db.clone();
            async move { db.lock().await.conn.execute_batch(q).unwrap() }
        };
        sql("ALTER TABLE maintenance_jobs RENAME TO maintenance_jobs_away").await;
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        sql("ALTER TABLE maintenance_jobs_away RENAME TO maintenance_jobs").await;
        tokio::time::sleep(std::time::Duration::from_millis(2800)).await;
        assert_eq!(
            db.lock().await.maintenance_requeue_abandoned().unwrap(),
            0,
            "the keeper stopped at a DB error and the lease lapsed"
        );
    }

    /// jobs-2: the per-page heartbeat read a DB ERROR as a lost lease, while
    /// the keeper retries the same error. The job then stopped with its gate
    /// cleared and its row still `running`.
    #[tokio::test]
    async fn heartbeat_does_not_read_a_db_error_as_a_lost_lease() {
        let (db, id) = claimed_job(60);
        db.lock()
            .await
            .conn
            .execute_batch("ALTER TABLE maintenance_jobs RENAME TO maintenance_jobs_away")
            .unwrap();
        assert_eq!(heartbeat(&db, id, Holder::test("inst")).await, Ok(()));
        // A refused renewal (another holder) still stops the phase.
        db.lock()
            .await
            .conn
            .execute_batch("ALTER TABLE maintenance_jobs_away RENAME TO maintenance_jobs")
            .unwrap();
        assert_eq!(
            heartbeat(&db, id, Holder::test("other")).await,
            Err(PhaseStop::LeaseLost)
        );
    }

    /// jobs-6: migrate cleanup pre-settles `completed_with_errors`; the audit
    /// must report that, not the `completed` that the row counters give.
    #[test]
    fn settled_status_prefers_the_rows_terminal_status() {
        assert_eq!(
            settled_status("completed", Some("completed_with_errors")),
            "completed_with_errors"
        );
        assert_eq!(settled_status("failed", Some("running")), "failed");
        assert_eq!(settled_status("completed", None), "completed");
    }

    /// A `counting` phase resumed from a persisted cursor keeps the count
    /// of the pages before the cursor, so `objects_total` is the whole
    /// bucket (it restarted at 0, and the bar reached 99 % early).
    #[tokio::test]
    async fn a_counting_resume_keeps_the_count_so_far() {
        let data = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let backend: Box<dyn crate::storage::StorageBackend> = Box::new(
            crate::storage::FilesystemBackend::new(data.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        );
        engine.create_bucket("b").await.unwrap();
        for k in ["a.txt", "b.txt", "c.txt"] {
            engine
                .store("b", k, b"x", None, Default::default())
                .await
                .unwrap();
        }
        let state = Arc::new(AppState {
            engine: arc_swap::ArcSwap::from_pointee(engine),
            multipart: Arc::new(crate::multipart::MultipartStore::new(
                config.max_object_size,
            )),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            usage_scanner: Arc::new(crate::usage_scanner::UsageScanner::new()),
            bucket_usage: None,
            reference_lock: None,
            config_db: None,
            maintenance_gate: Arc::new(crate::maintenance::gate::MaintenanceGate::new()),
            maintenance_notify: Arc::new(tokio::sync::Notify::new()),
            backend_capabilities: Default::default(),
            backend_health: Default::default(),
        });
        let db = ConfigDb::in_memory("testpass").unwrap();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("inst", current_unix_seconds(), 60)
            .unwrap()
            .unwrap();
        // The previous process counted a and b and persisted the filesystem
        // cursor "b.txt" (the token is the last key) before it died.
        db.maintenance_update_progress(id, "counting", Some(2), 0, 0, 0, 0, Some("b.txt"))
            .unwrap();
        let job = db.maintenance_job_by_id(id).unwrap().unwrap();
        let db = Arc::new(Mutex::new(db));
        let count = counting_phase(
            &db,
            &state,
            Holder::test("inst"),
            &job,
            job.continuation_token.clone(),
        )
        .await
        .unwrap();
        assert_eq!(
            count, 3,
            "the resumed count includes the objects before the cursor"
        );
    }

    #[test]
    fn after_run_never_settles_on_a_shutdown() {
        use AfterRun::*;
        let failed = || Err(PhaseStop::Failed("object list failed: x".into()));
        let cases: [(Result<(), PhaseStop>, bool, AfterRun); 10] = [
            (Ok(()), false, Settle),
            (Ok(()), true, Settle),
            (failed(), false, Settle),
            (failed(), true, ReleaseForResume),
            (Err(PhaseStop::ShuttingDown), false, ReleaseForResume),
            (Err(PhaseStop::ShuttingDown), true, ReleaseForResume),
            (Err(PhaseStop::LeaseLost), false, LeaveToLeaseHolder),
            (Err(PhaseStop::LeaseLost), true, LeaveToLeaseHolder),
            (Err(PhaseStop::Cancelled), false, Settle),
            (Err(PhaseStop::Cancelled), true, Settle),
        ];
        for (outcome, shutting_down, want) in cases {
            assert_eq!(
                after_run(&outcome, shutting_down),
                want,
                "{outcome:?} shutting_down={shutting_down}"
            );
        }
    }

    /// The row's `last_error` is the failure text, unchanged.
    #[test]
    fn a_failed_stop_keeps_its_message() {
        let e: PhaseStop = format!("copy of 'k' failed: {}", "boom").into();
        assert_eq!(e.to_string(), "copy of 'k' failed: boom");
        assert_eq!(PhaseStop::from("x"), PhaseStop::Failed("x".into()));
    }

    #[test]
    fn keeper_renews_three_times_per_default_ttl() {
        let t = LeaseTiming::MAINTENANCE;
        assert_eq!((t.ttl_secs, t.heartbeat_secs), (60, 20));
        let t = crate::config_sections::JobsConfig::default().lease_timing(t);
        assert_eq!((t.ttl_secs, t.heartbeat_secs), (60, 20));
    }
}
