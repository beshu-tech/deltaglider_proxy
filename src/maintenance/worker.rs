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
use crate::config::SharedConfig;
use crate::config_apply::ConfigMutator;
use crate::config_db::ConfigDb;
use crate::job_loop::Pager;
use crate::storage::encrypting::{ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY};
use crate::transfer::{copy_object_with_retries, ObjectTransferRequest};

use super::store::{current_unix_seconds, MaintenanceJob};
use super::{needs_rewrite, resolve_desired, strip_encryption_markers, DesiredEncryption};

const POLL_INTERVAL_SECS: u64 = 3;
const LEASE_TTL_SECS: i64 = 60;
pub(crate) const PAGE_SIZE: u32 = 1000;
const MAX_FAILURES_RETAINED: usize = 200;
const DRAIN_POLL_MS: u64 = 250;

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
                let claimed = {
                    let db = db.lock().await;
                    // Re-queue abandoned rows first (lease-aware): a job
                    // whose runner died mid-run becomes claimable within
                    // one lease TTL instead of waiting for the next boot.
                    if let Err(e) = db.maintenance_requeue_abandoned() {
                        warn!("maintenance: requeue scan failed: {}", e);
                    }
                    db.maintenance_claim_next_job(
                        &instance_id,
                        current_unix_seconds(),
                        LEASE_TTL_SECS,
                    )
                };
                match claimed {
                    Ok(Some(job)) => {
                        run_job(&mutator, &config, &db, &state, &instance_id, job).await;
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
    instance_id: &str,
    job: MaintenanceJob,
) {
    let bucket = job.bucket.clone();
    // The gate is armed at job creation and at boot; re-assert for safety
    // (idempotent) so a lost gate can never let writes race the rewrite.
    // For migrate jobs the TRANSIENT staging route is gated too (admin
    // copy/move endpoints could otherwise write through it mid-copy; the
    // S3 surface can't — s3s rejects `__`-named buckets at parse time).
    // EXCEPT: a migrate resumed in the `cleanup` phase must NOT re-arm — the
    // flip already happened, the bucket is fully live on the new backend, and
    // gating it would 503 client writes for the whole source-delete sweep.
    // The `flip` phase itself STAYS armed: a crash can persist phase="flip"
    // before the flip actually runs (bucket still routed to source), so writes
    // there must be gated or they are lost.
    let mut gated: Vec<String> = vec![bucket.clone()];
    if job.kind == "migrate" {
        if let Some(p) = job
            .params
            .as_deref()
            .and_then(|j| super::migrate::parse_params(j).ok())
        {
            gated.push(p.transient_key);
        }
    }
    let clear_gate = job.kind == "migrate" && !super::migrate::gate_armed_during(&job.phase);
    if clear_gate {
        // Resumed in cleanup: the gate may be armed from before the restart.
        for k in &gated {
            state.maintenance_gate.clear(k);
        }
    } else {
        for k in &gated {
            state.maintenance_gate.set_busy(k);
        }
    }

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

    let keeper = LeaseKeeper::spawn(
        db.clone(),
        job.id,
        instance_id.to_string(),
        LEASE_TTL_SECS,
        keeper_interval(LEASE_TTL_SECS),
    );
    let outcome = match job.kind.as_str() {
        "reencrypt" => execute_phases(config, db, state, instance_id, &job).await,
        "migrate" => {
            super::migrate::execute_migrate_phases(mutator, db, state, instance_id, &job).await
        }
        super::backfill::KIND => {
            super::backfill::execute_backfill_phases(db, state, instance_id, &job).await
        }
        other => Err(format!("unknown maintenance job kind '{other}'")),
    };
    drop(keeper);

    let action = after_run(&outcome, crate::shutdown::is_shutting_down());
    if action == AfterRun::ReleaseForResume {
        // A graceful shutdown (SIGTERM on every rolling deploy) stopped the
        // job, or made its work fail. Neither says anything about the data:
        // never settle. Hand the row back with its cursor, so the next boot
        // resumes it at once instead of after one lease TTL.
        let released = db
            .lock()
            .await
            .maintenance_release_for_resume(job.id, instance_id);
        warn!(
            "maintenance: job #{} on '{}' stopped by shutdown ({}) — left resumable \
             (released: {:?})",
            job.id,
            bucket,
            outcome.as_ref().err().map(String::as_str).unwrap_or(""),
            released
        );
        for k in &gated {
            state.maintenance_gate.clear(k);
        }
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
        let still_armed = db.lock().await.maintenance_gate_arm_keys();
        for k in gate_keys_to_clear_after_lease_loss(&gated, still_armed) {
            state.maintenance_gate.clear(&k);
        }
        return;
    }

    let (status, last_error) = match &outcome {
        Ok(()) => {
            // Per-object failures do not stop a job; they decide its status.
            let row = db.lock().await.maintenance_job_by_id(job.id).ok().flatten();
            row.map(|j| super::settle_status(j.objects_done, j.objects_skipped, j.objects_failed))
                .unwrap_or(("completed", None))
        }
        Err(e) if e == CANCELLED => ("cancelled", None),
        Err(e) => ("failed", Some(e.clone())),
    };
    // A phase can pre-settle its row (migrate cleanup: a note and
    // `completed_with_errors`); `maintenance_finish` then leaves it alone,
    // and the log + audit must report the row's status, not the recomputed one.
    let status = {
        let db = db.lock().await;
        if let Err(e) = db.maintenance_finish(job.id, status, last_error.as_deref()) {
            warn!("maintenance: failed to settle job #{}: {}", job.id, e);
        }
        let row = db.maintenance_job_by_id(job.id).ok().flatten();
        settled_status(status, row.as_ref().map(|j| j.status.as_str())).to_string()
    };
    for k in &gated {
        state.maintenance_gate.clear(k);
    }
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
    instance_id: &str,
    job: &MaintenanceJob,
) -> Result<(), String> {
    let bucket = &job.bucket;

    // ── Drain in-flight writes admitted before the gate armed. ──
    drain_inflight_writes(state, bucket).await?;

    let mut visitor = Reencrypt {
        config,
        bucket,
        desired: None,
    };
    let Counters {
        total,
        done,
        skipped,
        mut failed,
        bytes,
    } = run_count_then_walk(db, state, instance_id, job, "rewrite", &mut visitor).await?;
    let mut phase = job.phase.clone();
    if phase == "counting" || phase == "objects" {
        phase = "references".to_string();
        persist(db, job, &phase, total, done, skipped, failed, bytes, None).await;
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
                    failed += 1;
                }
            }
            heartbeat(db, job.id, instance_id).await?;
        }
        // Final counters (the per-reference failure increments above).
        persist(
            db,
            job,
            "references",
            total,
            done,
            skipped,
            failed,
            bytes,
            None,
        )
        .await;
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
    async fn begin_page(&mut self) -> Result<(), String> {
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
    ) -> Result<Visit, String> {
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
    async fn begin_page(&mut self) -> Result<(), String> {
        Ok(())
    }
    /// One user object, with the metadata the driver read for it.
    async fn visit(
        &mut self,
        engine: &Arc<crate::deltaglider::DynEngine>,
        key: &str,
        meta: &crate::types::FileMetadata,
    ) -> Result<Visit, String>;
}

/// A job's counters, as its row stores them.
pub(crate) struct Counters {
    pub total: Option<i64>,
    pub done: i64,
    pub skipped: i64,
    pub failed: i64,
    pub bytes: i64,
}

/// The `counting` → `objects` phases that the reencrypt and backfill kinds
/// share: page-granular resume, the poison-token restart, per-object
/// failures recorded and skipped over, cancel per page, shutdown per
/// object. Returns the counters at the end of `objects`; a job persisted in
/// a later phase gets its row's counters back untouched. `what` names the
/// job in the page-budget error.
pub(crate) async fn run_count_then_walk<V: ObjectVisitor>(
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    instance_id: &str,
    job: &MaintenanceJob,
    what: &str,
    visitor: &mut V,
) -> Result<Counters, String> {
    let bucket = &job.bucket;
    let mut phase = job.phase.clone();
    let resume_token = job.continuation_token.clone();
    let mut c = Counters {
        total: job.objects_total,
        done: job.objects_done,
        skipped: job.objects_skipped,
        failed: job.objects_failed,
        bytes: job.bytes_done,
    };

    // ── Phase: counting ──
    if phase == "counting" {
        let count = counting_phase(db, state, instance_id, job, resume_token.clone()).await?;
        c = Counters {
            total: Some(count),
            done: 0,
            skipped: 0,
            failed: 0,
            bytes: 0,
        };
        phase = "objects".to_string();
        persist(db, job, &phase, c.total, 0, 0, 0, 0, None).await;
    }

    // ── Phase: objects ──
    if phase != "objects" {
        return Ok(c);
    }
    // Resume only when the job was persisted IN this phase (a fresh
    // transition from counting starts at page 0).
    let mut pager = Pager::resuming(if job.phase == "objects" {
        resume_token
    } else {
        None
    });
    while pager.begin_page().is_some() {
        check_cancel(db, job.id).await?;
        visitor.begin_page().await?;
        let engine = state.engine.load().clone();
        let page = match engine
            .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
            .await
        {
            Ok(p) => p,
            Err(e) if pager.poisoned_resume_token() => {
                // Restart the phase from page 0: the visitor skips objects
                // already done, so the re-scan is idempotent. Counters are
                // NOT reset: re-encountered objects tally as `skipped`
                // again — display drift only, never a second rewrite.
                warn!(
                    "maintenance: job #{} objects resume token rejected ({e}); restarting phase fresh",
                    job.id
                );
                pager.restart_fresh();
                persist(
                    db, job, "objects", c.total, c.done, c.skipped, c.failed, c.bytes, None,
                )
                .await;
                continue;
            }
            Err(e) => return Err(format!("object list failed: {e}")),
        };

        for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
            stop_if_shutting_down()?;
            let visit = match engine.head(bucket, key).await {
                Ok(meta) => visitor.visit(&engine, key, &meta).await?,
                Err(e) => Visit::Failed(format!("could not read object metadata: {e}")),
            };
            match visit {
                Visit::Done { bytes } => {
                    c.done += 1;
                    c.bytes += bytes;
                }
                Visit::Skipped => c.skipped += 1,
                Visit::Failed(reason) => {
                    record_failure(db, job.id, key, &reason).await?;
                    c.failed += 1;
                }
            }
        }

        let more = pager.advance(page.is_truncated, page.next_continuation_token);
        persist(
            db,
            job,
            "objects",
            c.total,
            c.done,
            c.skipped,
            c.failed,
            c.bytes,
            pager.token(),
        )
        .await;
        heartbeat(db, job.id, instance_id).await?;
        if !more {
            break;
        }
    }
    if pager.truncated_by_page_budget() {
        // Falling through would report `completed` with the tail still
        // unprocessed — silent truncation.
        return Err(format!(
            "{what} stopped at the page budget with more pages pending — bucket \
             too large for one pass; job left resumable in phase 'objects' \
             (cursor persisted)"
        ));
    }
    Ok(c)
}

/// The shared `counting` phase: one LIST sweep for the exact object total
/// (the write gate freezes the write set, so the total cannot drift and
/// the progress bar is honest). Persists progress + cursor per page; used
/// by the reencrypt AND backfill-metadata kinds.
pub(crate) async fn counting_phase(
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    instance_id: &str,
    job: &MaintenanceJob,
    resume_token: Option<String>,
) -> Result<i64, String> {
    let bucket = &job.bucket;
    // A resumed count keeps the pages before its cursor: the row's total
    // is the count up to that cursor (persisted with it, page by page).
    let mut count: i64 = match (&resume_token, job.phase.as_str()) {
        (Some(_), "counting") => job.objects_total.unwrap_or(0),
        _ => 0,
    };
    let mut pager = Pager::resuming(resume_token);
    while pager.begin_page().is_some() {
        check_cancel(db, job.id).await?;
        let engine = state.engine.load().clone();
        let page = match engine
            .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
            .await
        {
            Ok(p) => p,
            Err(e) if pager.poisoned_resume_token() => {
                // The persisted cursor is the prime suspect — drop it,
                // persist the clean cursor (a crash mid-retry must not
                // re-poison), and recount from page 0 (idempotent).
                warn!(
                    "maintenance: job #{} counting resume token rejected ({e}); restarting phase fresh",
                    job.id
                );
                pager.restart_fresh();
                count = 0;
                persist(db, job, "counting", Some(0), 0, 0, 0, 0, None).await;
                continue;
            }
            Err(e) => return Err(format!("counting list failed: {e}")),
        };
        count += page
            .objects
            .iter()
            .filter(|(k, _)| !k.ends_with('/'))
            .count() as i64;
        let more = pager.advance(page.is_truncated, page.next_continuation_token);
        persist(db, job, "counting", Some(count), 0, 0, 0, 0, pager.token()).await;
        heartbeat(db, job.id, instance_id).await?;
        if !more {
            break;
        }
    }
    if pager.truncated_by_page_budget() {
        return Err("counting stopped at the page budget with more pages \
             pending — bucket too large for one pass; job left resumable"
            .to_string());
    }
    Ok(count)
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
pub(crate) async fn check_cancel(db: &Arc<Mutex<ConfigDb>>, job_id: i64) -> Result<(), String> {
    let db = db.lock().await;
    match db.maintenance_cancel_requested(job_id) {
        Ok(true) => Err(CANCELLED.to_string()),
        _ => Ok(()),
    }
}
pub(crate) const CANCELLED: &str = "__cancelled__";
/// Sentinel for "this worker's lease was not renewed". The job row is
/// left UNTOUCHED (no settle, no unwind) — it belongs to whoever holds
/// the lease now, or to the requeue scan once it lapses.
pub(crate) const LEASE_LOST: &str = "__lease_lost__";
/// Sentinel for "the process shuts down". Like [`LEASE_LOST`], the row is
/// not settled; the worker hands it back for the next boot to resume.
pub(crate) const SHUTTING_DOWN: &str = "__shutting_down__";

/// Stop point for phase loops: `Err(SHUTTING_DOWN)` once shutdown starts.
pub(crate) fn stop_if_shutting_down() -> Result<(), String> {
    if crate::shutdown::is_shutting_down() {
        Err(SHUTTING_DOWN.to_string())
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
pub(crate) fn after_run(outcome: &Result<(), String>, shutting_down: bool) -> AfterRun {
    match outcome {
        Ok(()) => AfterRun::Settle,
        Err(e) if e == LEASE_LOST => AfterRun::LeaveToLeaseHolder,
        Err(e) if e == SHUTTING_DOWN => AfterRun::ReleaseForResume,
        Err(e) if e == CANCELLED => AfterRun::Settle,
        Err(_) if shutting_down => AfterRun::ReleaseForResume,
        Err(_) => AfterRun::Settle,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist(
    db: &Arc<Mutex<ConfigDb>>,
    job: &MaintenanceJob,
    phase: &str,
    total: Option<i64>,
    done: i64,
    skipped: i64,
    failed: i64,
    bytes: i64,
    token: Option<&str>,
) {
    let db = db.lock().await;
    if let Err(e) =
        db.maintenance_update_progress(job.id, phase, total, done, skipped, failed, bytes, token)
    {
        warn!(
            "maintenance: progress persist failed for job #{}: {}",
            job.id, e
        );
    }
}

/// Renews a job lease on a timer for as long as it lives (aborted on drop).
/// The per-page `heartbeat` is not enough on its own: one page of copy work
/// can outlast the TTL, and a lapsed lease lets the requeue scan hand the job
/// back and the write gate open mid-job. The keeper stops at the first
/// refused renewal (a DB error is retried); the next per-page `heartbeat`
/// then reports LEASE_LOST.
pub(crate) struct LeaseKeeper(tokio::task::JoinHandle<()>);

impl LeaseKeeper {
    pub(crate) fn spawn(
        db: Arc<Mutex<ConfigDb>>,
        job_id: i64,
        instance_id: String,
        ttl_secs: i64,
        interval: std::time::Duration,
    ) -> Self {
        Self(tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let renewed = {
                    let db = db.lock().await;
                    db.maintenance_heartbeat(job_id, &instance_id, current_unix_seconds(), ttl_secs)
                };
                use crate::config_db::job_store::{keeper_step, KeeperStep};
                match keeper_step(&renewed) {
                    KeeperStep::Held => {}
                    KeeperStep::Lost => {
                        warn!("maintenance: job #{job_id} lease renewal refused; keeper stops");
                        return;
                    }
                    // If the DB stays unreadable past the TTL, the per-page
                    // heartbeat then stops the phase.
                    KeeperStep::Retry => warn!(
                        "maintenance: job #{job_id} lease renewal failed ({:?}); retrying",
                        renewed.err()
                    ),
                }
            }
        }))
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Renew three times per TTL, so one slow renewal never lapses the lease.
fn keeper_interval(ttl_secs: i64) -> std::time::Duration {
    std::time::Duration::from_millis((ttl_secs.max(1) as u64) * 1000 / 3)
}

/// Renew the job lease; `Err(LEASE_LOST)` means the renewal was refused
/// (lapsed, or taken by another instance) and the phase MUST stop — this
/// is the one subsystem that flips config and deletes source data, so a
/// lapsed worker must never keep going. A DB error is NOT a refusal (same
/// verdict as the keeper): stopping on it cleared the write gate while the
/// row stayed active. A lapsed lease never renews, so once the DB answers
/// again a lapse still reads as LEASE_LOST.
pub(crate) async fn heartbeat(
    db: &Arc<Mutex<ConfigDb>>,
    job_id: i64,
    instance_id: &str,
) -> Result<(), String> {
    let renewed = {
        let db = db.lock().await;
        db.maintenance_heartbeat(job_id, instance_id, current_unix_seconds(), LEASE_TTL_SECS)
    };
    use crate::config_db::job_store::{keeper_step, KeeperStep};
    match keeper_step(&renewed) {
        KeeperStep::Held => Ok(()),
        KeeperStep::Lost => Err(LEASE_LOST.to_string()),
        KeeperStep::Retry => {
            warn!(
                "maintenance: job #{job_id} per-page lease renewal failed ({:?}); \
                 the keeper retries it",
                renewed.err()
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

/// Gate keys a job may clear after it lost its lease: only the keys that
/// no ACTIVE row still needs (`maintenance_gate_arm_keys` is the truth).
/// The row stays active after a lapse (the requeue scan hands it back), so
/// clearing its gate let writes race a pre-flip migrate. An unreadable DB
/// clears nothing (fail closed: the next claim or boot re-derives it).
pub(crate) fn gate_keys_to_clear_after_lease_loss(
    gated: &[String],
    still_armed: Result<Vec<String>, crate::config_db::ConfigDbError>,
) -> Vec<String> {
    match still_armed {
        Ok(armed) => gated
            .iter()
            .filter(|k| !armed.contains(k))
            .cloned()
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Record a per-object failure. Refuses with `Err(SHUTTING_DOWN)` once the
/// process shuts down: a failure then is most likely the runtime teardown,
/// not the object, and counting it would settle a healthy job with errors.
/// Callers propagate the Err so the phase stops before its page persists.
pub(crate) async fn record_failure(
    db: &Arc<Mutex<ConfigDb>>,
    job_id: i64,
    key: &str,
    error: &str,
) -> Result<(), String> {
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
        let _keeper = LeaseKeeper::spawn(
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
        let _keeper = LeaseKeeper::spawn(
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
        assert_eq!(heartbeat(&db, id, "inst").await, Ok(()));
        // A refused renewal (another holder) still stops the phase.
        db.lock()
            .await
            .conn
            .execute_batch("ALTER TABLE maintenance_jobs_away RENAME TO maintenance_jobs")
            .unwrap();
        assert_eq!(
            heartbeat(&db, id, "other").await,
            Err(LEASE_LOST.to_string())
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

    /// jobs-2: a job that lost its lease keeps the gate keys an active row
    /// still needs (a pre-flip migrate: bucket + staging route).
    #[test]
    fn lease_loss_keeps_the_gate_of_an_active_row() {
        let db = ConfigDb::in_memory("testpass").unwrap();
        let params = r#"{"target_backend":"t","delete_source":true,"transient_key":"__dgmigrate_b_0","from_backend":"s"}"#;
        db.maintenance_create_job("migrate", "b", "copy", Some(params), "admin", 1)
            .unwrap()
            .unwrap();
        let gated = vec!["b".to_string(), "__dgmigrate_b_0".to_string()];
        assert!(
            gate_keys_to_clear_after_lease_loss(&gated, db.maintenance_gate_arm_keys()).is_empty()
        );
        // No active row → the keys clear.
        let empty = ConfigDb::in_memory("testpass").unwrap();
        assert_eq!(
            gate_keys_to_clear_after_lease_loss(&gated, empty.maintenance_gate_arm_keys()),
            gated
        );
        // An unreadable DB clears nothing.
        assert!(gate_keys_to_clear_after_lease_loss(
            &gated,
            Err(crate::config_db::ConfigDbError::Other("x".into()))
        )
        .is_empty());
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
        let count = counting_phase(&db, &state, "inst", &job, job.continuation_token.clone())
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
        let err = |e: &str| Err(e.to_string());
        let cases: [(Result<(), String>, bool, AfterRun); 10] = [
            (Ok(()), false, Settle),
            (Ok(()), true, Settle),
            (err("object list failed: x"), false, Settle),
            (err("object list failed: x"), true, ReleaseForResume),
            (err(SHUTTING_DOWN), false, ReleaseForResume),
            (err(SHUTTING_DOWN), true, ReleaseForResume),
            (err(LEASE_LOST), false, LeaveToLeaseHolder),
            (err(LEASE_LOST), true, LeaveToLeaseHolder),
            (err(CANCELLED), false, Settle),
            (err(CANCELLED), true, Settle),
        ];
        for (outcome, shutting_down, want) in cases {
            assert_eq!(
                after_run(&outcome, shutting_down),
                want,
                "{outcome:?} shutting_down={shutting_down}"
            );
        }
    }

    #[test]
    fn keeper_interval_is_well_inside_the_ttl() {
        assert!(
            keeper_interval(LEASE_TTL_SECS) * 3
                <= std::time::Duration::from_secs(LEASE_TTL_SECS as u64)
        );
        assert_eq!(keeper_interval(1), std::time::Duration::from_millis(333));
    }
}
