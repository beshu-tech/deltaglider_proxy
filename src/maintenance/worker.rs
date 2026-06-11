// SPDX-License-Identifier: GPL-3.0-only

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
const PAGE_SIZE: u32 = 1000;
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
            }
            // Drain every claimable job before sleeping again.
            loop {
                let claimed = {
                    let db = db.lock().await;
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
    state.maintenance_gate.set_busy(&bucket);

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

    let outcome = match job.kind.as_str() {
        "reencrypt" => execute_phases(config, db, state, instance_id, &job).await,
        "migrate" => {
            super::migrate::execute_migrate_phases(mutator, db, state, instance_id, &job).await
        }
        other => Err(format!("unknown maintenance job kind '{other}'")),
    };

    let (status, last_error) = match &outcome {
        Ok(()) => ("completed", None),
        Err(e) if e == CANCELLED => ("cancelled", None),
        Err(e) => ("failed", Some(e.clone())),
    };
    {
        let db = db.lock().await;
        if let Err(e) = db.maintenance_finish(job.id, status, last_error.as_deref()) {
            warn!("maintenance: failed to settle job #{}: {}", job.id, e);
        }
    }
    state.maintenance_gate.clear(&bucket);
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

    let mut phase = job.phase.clone();
    let resume_token = job.continuation_token.clone();
    let mut total = job.objects_total;
    let mut done = job.objects_done;
    let mut skipped = job.objects_skipped;
    let mut failed = job.objects_failed;
    let mut bytes = job.bytes_done;

    // ── Phase: counting ──
    if phase == "counting" {
        let mut count: i64 = 0;
        let mut pager = Pager::resuming(resume_token.clone());
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
            heartbeat(db, job.id, instance_id).await;
            if !more {
                break;
            }
        }
        total = Some(count);
        phase = "objects".to_string();
        (done, skipped, failed, bytes) = (0, 0, 0, 0);
        persist(db, job, &phase, total, 0, 0, 0, 0, None).await;
    }

    // ── Phase: objects ──
    if phase == "objects" {
        // Resume only when the job was persisted IN this phase (a fresh
        // transition from counting starts at page 0).
        let mut pager = Pager::resuming(if job.phase == "objects" {
            resume_token.clone()
        } else {
            None
        });
        while pager.begin_page().is_some() {
            check_cancel(db, job.id).await?;
            // Re-resolve the desired state every page: a config apply
            // mid-run swaps the engine; the job must follow (or abort if
            // the mode became unsupported).
            let desired = {
                let cfg = config.read().await;
                resolve_desired(&cfg, bucket).map_err(|e| format!("config changed mid-run: {e}"))?
            };
            let engine = state.engine.load().clone();
            let page = match engine
                .list_objects(bucket, "", None, PAGE_SIZE, pager.token(), false)
                .await
            {
                Ok(p) => p,
                Err(e) if pager.poisoned_resume_token() => {
                    // Restart the phase from page 0: needs_rewrite makes the
                    // re-scan idempotent (already-rewritten objects skip).
                    warn!(
                        "maintenance: job #{} objects resume token rejected ({e}); restarting phase fresh",
                        job.id
                    );
                    pager.restart_fresh();
                    persist(
                        db, job, "objects", total, done, skipped, failed, bytes, None,
                    )
                    .await;
                    continue;
                }
                Err(e) => return Err(format!("object list failed: {e}")),
            };

            for (key, _) in page.objects.iter().filter(|(k, _)| !k.ends_with('/')) {
                let meta = match engine.head(bucket, key).await {
                    Ok(m) => m,
                    Err(e) => {
                        failed += 1;
                        record_failure(db, job.id, key, &format!("head failed: {e}")).await;
                        continue;
                    }
                };
                if !needs_rewrite(&meta.user_metadata, &desired) {
                    skipped += 1;
                    continue;
                }
                let req = ObjectTransferRequest {
                    source_bucket: bucket,
                    source_key: key,
                    destination_bucket: bucket,
                    destination_key: key,
                    provenance: None,
                    // Shed stale markers; the encrypting wrapper re-stamps
                    // fresh ones when the destination mode encrypts.
                    strip_user_metadata_keys: &[ENCRYPTION_MARKER_KEY, ENCRYPTION_KEY_ID_KEY],
                    operation: "maintenance-reencrypt",
                };
                match copy_object_with_retries(&engine, req).await {
                    Ok(outcome) => {
                        done += 1;
                        bytes += outcome.bytes_copied as i64;
                    }
                    Err(e) => {
                        failed += 1;
                        record_failure(db, job.id, key, &e.to_string()).await;
                    }
                }
            }

            let more = pager.advance(page.is_truncated, page.next_continuation_token);
            persist(
                db,
                job,
                "objects",
                total,
                done,
                skipped,
                failed,
                bytes,
                pager.token(),
            )
            .await;
            heartbeat(db, job.id, instance_id).await;
            if !more {
                break;
            }
        }
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
        let storage: &dyn crate::storage::StorageBackend = engine.storage().as_ref();
        let deltaspaces = storage
            .list_deltaspaces(bucket)
            .await
            .map_err(|e| format!("list deltaspaces failed: {e}"))?;
        for prefix in deltaspaces {
            check_cancel(db, job.id).await?;
            match rewrite_reference_if_needed(storage, bucket, &prefix, &desired).await {
                Ok(()) => {}
                Err(e) => {
                    failed += 1;
                    record_failure(db, job.id, &format!("{prefix}/.dg/reference.bin"), &e).await;
                }
            }
            heartbeat(db, job.id, instance_id).await;
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

/// Re-store a deltaspace's reference blob through the (encrypting)
/// storage wrapper when its at-rest state doesn't match. The reference's
/// PLAINTEXT bytes are unchanged, so the engine's in-memory
/// ReferenceCache (keyed by content) stays valid.
async fn rewrite_reference_if_needed(
    storage: &dyn crate::storage::StorageBackend,
    bucket: &str,
    prefix: &str,
    desired: &DesiredEncryption,
) -> Result<(), String> {
    if !storage.has_reference(bucket, prefix).await {
        return Ok(());
    }
    let meta = storage
        .get_reference_metadata(bucket, prefix)
        .await
        .map_err(|e| format!("reference metadata failed: {e}"))?;
    if !needs_rewrite(&meta.user_metadata, desired) {
        return Ok(());
    }
    let data = storage
        .get_reference(bucket, prefix)
        .await
        .map_err(|e| format!("reference read failed: {e}"))?;
    let mut new_meta = meta;
    strip_encryption_markers(&mut new_meta.user_metadata);
    storage
        .put_reference(bucket, prefix, &data, &new_meta)
        .await
        .map_err(|e| format!("reference rewrite failed: {e}"))?;
    Ok(())
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

pub(crate) async fn heartbeat(db: &Arc<Mutex<ConfigDb>>, job_id: i64, instance_id: &str) {
    let db = db.lock().await;
    let _ = db.maintenance_heartbeat(job_id, instance_id, current_unix_seconds(), LEASE_TTL_SECS);
}

pub(crate) async fn record_failure(db: &Arc<Mutex<ConfigDb>>, job_id: i64, key: &str, error: &str) {
    let db = db.lock().await;
    if let Err(e) = db.maintenance_record_failure(job_id, key, error, MAX_FAILURES_RETAINED) {
        warn!(
            "maintenance: failure record failed for job #{}: {}",
            job_id, e
        );
    }
}
