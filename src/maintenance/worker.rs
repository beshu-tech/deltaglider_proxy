// SPDX-License-Identifier: BUSL-1.1

//! The maintenance runner: a single background task that claims queued
//! jobs (oldest first, one at a time — bounded resource usage) and
//! executes the canned re-encryption procedure:
//!
//! 1. **drain** — wait for the gated bucket's in-flight S3 writes to
//!    reach zero (the gate rejects NEW writes, but one admitted moments
//!    before it armed could otherwise land mid-rewrite and be lost).
//! 2. **counting** — no listing: the total is the bucket's usage counter
//!    when a full scan stamped it, else unknown (an indeterminate bar)
//!    until the objects walk ends with the exact count.
//! 3. **objects** — per object: `engine.head_listed` (one HEAD of the
//!    stored form that the page's listing proved) → [`needs_rewrite`]? then
//!    rewrite in place via `transfer::copy_object_known` (that HEAD is the
//!    copy's source and the write's prior)
//!    (source == destination; the engine's store path encrypts/decrypts
//!    per the CURRENT backend mode; stale markers stripped). Failures are
//!    recorded per object and the job continues.
//! 4. **references** — the deltaspaces whose `reference.bin` the objects
//!    walk read (a resumed job lists them). These blobs are shared
//!    storage-level artifacts the object sweep never rewrites; re-store
//!    them through the (encrypting) storage wrapper when their state
//!    doesn't match.
//!
//! Progress + the continuation token are persisted after every page, so
//! a crash/restart resumes mid-bucket (boot reconcile re-queues the job
//! with its cursor intact). Cancellation is checked per page.
//!
//! [`needs_rewrite`]: super::needs_rewrite

use crate::storage::StorageBackend;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::api::handlers::AppState;
use crate::background::LeaseKeeper;
use crate::config::SharedConfig;
use crate::config_apply::ConfigMutator;
use crate::config_db::JobsDb;
use crate::config_sections::LeaseTiming;
use crate::coordination::LeaseError;
use crate::deltaglider::Rewritten;
use crate::storage::ObjectVariant;

use super::paged::{
    list_job_keys, paged_phase, BackgroundCount, JobCtx, KeyPage, ListError, PageStep, PhaseSpec,
    CANCEL_CHECK_EVERY, PAGE_CONCURRENCY,
};
use super::store::{current_unix_seconds, Lane, MaintenanceJob};
use super::{needs_rewrite, resolve_desired, DesiredEncryption};

/// How long a job whose backend is gated waits before a worker claims it
/// again (about one health re-probe interval).
const BACKEND_DEFER_SECS: i64 = 30;
/// How many times a job that a backend's transient failure stopped (a
/// timeout, a 5xx or a throttle, after the in-place retries) goes back to
/// the queue with its cursor, without progress in between, before it
/// settles `failed`.
pub(crate) const TRANSIENT_RETRIES: u32 = 5;
/// The wait before the first such retry; it doubles each time.
const TRANSIENT_RETRY_SECS: i64 = 60;
pub(crate) const PAGE_SIZE: u32 = 1000;
const MAX_FAILURES_RETAINED: usize = 200;
const DRAIN_POLL_MS: u64 = 250;
/// Objects of a page that the re-encrypt job converts at once (the cursor
/// is saved after the whole page).
pub(crate) const REENCRYPT_CONCURRENCY: usize = 8;

/// The conversion job's read rule (`storage/encrypting/conversion.rs`):
/// its reads serve every unsealed object of the bucket, also those that a
/// client may not read, and report them. A [`ConversionReadProof`] opens
/// that scope, and only this module makes one (source guard
/// `only_the_conversion_job_reads_as_the_job`). Two jobs enter it: the
/// re-encrypt job, and the migrate job (the way out of a read-only bucket
/// whose plaintext history no client may read).
mod conversion_reads {
    /// Proof that the reads in a scope are the re-encrypt job's.
    #[derive(Debug)]
    pub struct ConversionReadProof {
        _private: (),
    }

    pub(super) const PROOF: &ConversionReadProof = &ConversionReadProof { _private: () };

    impl ConversionReadProof {
        /// For lib tests that drive the encrypting wrapper directly.
        #[cfg(test)]
        pub(crate) fn for_tests() -> &'static ConversionReadProof {
            PROOF
        }
    }
}

pub use conversion_reads::ConversionReadProof;

/// Who holds a job's lease, and its timing (`advanced.jobs`, read at
/// claim). The claim, the keeper and the per-page heartbeat all renew
/// with the same TTL.
#[derive(Clone, Copy)]
pub(crate) struct Holder<'a> {
    pub id: &'a str,
    pub lease: LeaseTiming,
    /// The job's backend-health check (`None`: never gated).
    pub backend: Option<&'a super::gate::BackendCheck>,
    /// What a delete job needs (`None` in tests that run no delete).
    pub deletes: Option<&'a super::delete::DeleteDeps>,
}

impl Holder<'_> {
    /// `Err(Deferred)` while a backend of the job is gated: the job waits
    /// instead of sending its requests to a backend that cannot answer.
    pub(crate) fn check_backend(&self) -> Result<(), PhaseStop> {
        match self.backend.and_then(super::gate::BackendCheck::gated) {
            Some(why) => Err(PhaseStop::Deferred(why)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
impl<'a> Holder<'a> {
    pub(crate) fn test(id: &'a str) -> Self {
        Self {
            id,
            lease: LeaseTiming::MAINTENANCE,
            backend: None,
            deletes: None,
        }
    }
}

/// Spawn the maintenance worker: two lanes over one queue. The serial lane
/// runs every kind but `delete`, one job at a time (a re-encryption or a
/// migration rewrites a whole bucket). The delete lane runs delete jobs,
/// [`super::delete::DELETE_PARALLEL`] at a time: deletes on buckets of
/// different backends go in parallel, and a long re-encryption never
/// holds them up.
///
/// Each lane drains every job it may claim, then waits for
/// `state.maintenance_notify` (every job creation sends it), for the time
/// at which a deferred, backed-off or abandoned row comes due, or at most
/// half a lease TTL (`idle_wait_secs`). It drains at once on start: a job
/// that the previous process left queued resumes at boot.
pub fn spawn_worker(
    mutator: ConfigMutator,
    db: Arc<Mutex<JobsDb>>,
    deletes: super::delete::DeleteDeps,
) -> tokio::task::JoinHandle<()> {
    crate::background::spawn_supervised("maintenance-worker", move || {
        let (mutator, db, deletes) = (mutator.clone(), db.clone(), deletes.clone());
        async move {
            // One task: aborting the worker stops both lanes.
            tokio::join!(
                run_lane(
                    mutator.clone(),
                    db.clone(),
                    deletes.clone(),
                    Lane::Serial,
                    1
                ),
                run_lane(
                    mutator,
                    db,
                    deletes,
                    Lane::Delete,
                    super::delete::DELETE_PARALLEL,
                ),
            );
        }
    })
}

async fn run_lane(
    mutator: ConfigMutator,
    db: Arc<Mutex<JobsDb>>,
    deletes: super::delete::DeleteDeps,
    lane: Lane,
    parallel: usize,
) {
    let config: SharedConfig = mutator.config.clone();
    let state: Arc<AppState> = mutator.app.clone();
    let instance_id: Arc<str> = format!("maintenance:{}", uuid::Uuid::new_v4()).into();
    let slots = Arc::new(tokio::sync::Semaphore::new(parallel.max(1)));
    {
        info!(
            "Maintenance worker lane {:?} started: instance_id={}, parallel={}",
            lane, instance_id, parallel
        );
        loop {
            // Drain every claimable job before waiting again.
            let (lease, next_due) = loop {
                // A free slot first: a lane with every slot busy claims
                // nothing, so the row stays for a peer or for later.
                let Ok(slot) = slots.clone().acquire_owned().await else {
                    return;
                };
                if crate::shutdown::is_shutting_down() {
                    // The jobs of this lane run in their own tasks: wait until
                    // each one has handed its row back (`ReleaseForResume`),
                    // so the next boot resumes it at once instead of waiting
                    // for its lease to lapse. main bounds the wait.
                    drop(slot);
                    let _ = slots.acquire_many(parallel.max(1) as u32).await;
                    info!(
                        "Maintenance worker lane {:?} stopped: the process shuts down",
                        lane
                    );
                    return;
                }
                let lease = config
                    .read()
                    .await
                    .jobs
                    .lease_timing(LeaseTiming::MAINTENANCE);
                let (claimed, next_due) = {
                    let db = db.lock().await;
                    // Re-queue abandoned rows first (lease-aware): a job
                    // whose runner died mid-run becomes claimable within
                    // one lease TTL instead of waiting for the next boot.
                    if let Err(e) = db.maintenance_requeue_abandoned() {
                        warn!("maintenance: requeue scan failed: {}", e);
                    }
                    if let Err(e) = db.maintenance_prune_finished(current_unix_seconds()) {
                        warn!("maintenance: job retention failed: {}", e);
                    }
                    let claimed = db.maintenance_claim_next_job_in(
                        lane,
                        &instance_id,
                        current_unix_seconds(),
                        lease.ttl_secs,
                    );
                    // Every tick re-derives the gate: the claim (a migrate
                    // resumed in `cleanup` gates nothing), the requeue, and
                    // any transition a crashed path did not sync.
                    state.maintenance_gate.sync_from(&db);
                    let next_due = match claimed {
                        Ok(None) => db
                            .maintenance_next_due_at(current_unix_seconds())
                            .unwrap_or_else(|e| {
                                warn!("maintenance: next due time unknown: {}", e);
                                None
                            }),
                        _ => None,
                    };
                    (claimed, next_due)
                };
                match claimed {
                    Ok(Some(job)) => {
                        let (mutator, config, db, state, deletes, id) = (
                            mutator.clone(),
                            config.clone(),
                            db.clone(),
                            state.clone(),
                            deletes.clone(),
                            instance_id.clone(),
                        );
                        tokio::spawn(async move {
                            let holder = Holder {
                                id: &id,
                                lease,
                                backend: None,
                                deletes: Some(&deletes),
                            };
                            run_job(&mutator, &config, &db, &state, holder, job).await;
                            drop(slot);
                        });
                    }
                    Ok(None) => break (lease, next_due),
                    Err(e) => {
                        warn!("maintenance: claim failed: {}", e);
                        break (lease, None);
                    }
                }
            };
            let wait = idle_wait_secs(lease.ttl_secs, next_due, current_unix_seconds());
            tokio::select! {
                _ = state.maintenance_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
                _ = crate::shutdown::started() => {}
            }
        }
    }
}

/// Pure: how long the idle worker waits before it looks at the queue
/// again without a notify. Half a lease TTL at most (an abandoned row of a
/// peer becomes claimable within one TTL; a synced row that no notify
/// announces is seen within half of one); until `next_due` (the earliest
/// time at which a row comes due by time alone) when that is sooner; never
/// below one second.
pub(crate) fn idle_wait_secs(lease_ttl_secs: i64, next_due: Option<i64>, now: i64) -> u64 {
    const MIN_WAIT_SECS: i64 = 1;
    let poll = (lease_ttl_secs / 2).max(MIN_WAIT_SECS);
    let wait = match next_due {
        Some(at) => (at - now).clamp(MIN_WAIT_SECS, poll),
        None => poll,
    };
    wait as u64
}

/// Execute one claimed job to a terminal state. Never panics the loop:
/// every failure path settles the row and releases the gate.
async fn run_job(
    mutator: &ConfigMutator,
    config: &SharedConfig,
    db: &Arc<Mutex<JobsDb>>,
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
    let check = backend_check(config, state, &job);
    let holder = Holder {
        backend: Some(&check),
        ..holder
    };
    // A row on the coordination bucket (queued by an older release, which
    // admitted it) fails before any phase touches that bucket (review B3).
    let reserved = state
        .engine
        .load()
        .bucket_policy_registry()
        .client_bucket(&bucket)
        .err();
    // A gated backend: wait before any phase sends a request.
    let gated = holder.check_backend();
    let outcome = match job.kind.as_str() {
        _ if reserved.is_some() => Err(reserved.unwrap_or_default().into()),
        _ if gated.is_err() => gated,
        // The conversion job reads every unsealed object of its bucket
        // (`storage/encrypting/conversion.rs`): only here.
        "reencrypt" => {
            crate::storage::encrypting::conversion::as_conversion_job(
                conversion_reads::PROOF,
                execute_phases(config, db, state, holder, &job),
            )
            .await
        }
        // A migrate reads its source as the job too: a read-only bucket
        // with plaintext history moves out (compatibility review C1); the
        // copy reports each unsealed object it encrypts on the target.
        "migrate" => {
            crate::storage::encrypting::conversion::as_conversion_job(
                conversion_reads::PROOF,
                // Boxed: the migrate future is large, and the scope must not
                // carry it on the worker's stack (a debug build overflowed it).
                Box::pin(super::migrate::execute_migrate_phases(
                    mutator, db, state, holder, &job,
                )),
            )
            .await
        }
        super::backfill::KIND => {
            super::backfill::execute_backfill_phases(db, state, holder, &job).await
        }
        super::delete::KIND => {
            super::delete::execute_delete_phases(config, db, state, holder, &job).await
        }
        other => Err(format!("unknown maintenance job kind '{other}'").into()),
    };
    drop(keeper);

    let row = current_row(db, &job).await;
    let action = after_run(
        &outcome,
        crate::shutdown::is_shutting_down(),
        transient_retry_left(&row),
    );
    if action == AfterRun::Defer {
        // A backend of the job is gated, or did not answer. The row goes
        // back to `queued` with its cursor, and no worker claims it for a
        // while: the job sends nothing to the backend meanwhile. The row
        // stays active, so its write gate stays armed.
        let (wait, why) = match &outcome {
            Err(PhaseStop::FailedTransient(e)) => {
                let n = note_transient_stop(&row);
                (
                    TRANSIENT_RETRY_SECS << (n - 1).min(6),
                    format!(
                        "{e} — the job runs again from its cursor (retry {n} of \
                         {TRANSIENT_RETRIES})"
                    ),
                )
            }
            other => (
                BACKEND_DEFER_SECS,
                other
                    .as_ref()
                    .err()
                    .map(PhaseStop::to_string)
                    .unwrap_or_default(),
            ),
        };
        let deferred = {
            let db = db.lock().await;
            let deferred = db.maintenance_defer(
                job.id,
                holder.id,
                current_unix_seconds().saturating_add(wait),
                &why,
            );
            state.maintenance_gate.sync_from(&db);
            deferred
        };
        info!(
            "maintenance: job #{} on '{}' waits {wait}s ({why}) (deferred: {:?})",
            job.id, bucket, deferred
        );
        return;
    }
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

    forget_transient_stops(&job);
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

/// The row of `job` as the DB holds it now (the claimed row when the DB
/// cannot answer).
pub(crate) async fn current_row(db: &Arc<Mutex<JobsDb>>, job: &MaintenanceJob) -> MaintenanceJob {
    db.lock()
        .await
        .maintenance_job_by_id(job.id)
        .ok()
        .flatten()
        .unwrap_or_else(|| job.clone())
}

/// What a job's row shows of its progress.
type ProgressMark = (String, Option<String>, i64);

fn progress_mark(row: &MaintenanceJob) -> ProgressMark {
    (
        row.phase.clone(),
        row.continuation_token.clone(),
        row.objects_done + row.objects_skipped,
    )
}

/// A job, as the transient-stop count knows it: the id, and what makes a
/// row with a reused id (a restored DB, the in-memory DBs of tests) another
/// job.
type JobIdentity = (i64, String, String, i64);

fn identity(row: &MaintenanceJob) -> JobIdentity {
    (row.id, row.kind.clone(), row.bucket.clone(), row.created_at)
}

/// The transient stops of each job in this process: how many since the
/// job last made progress, and its progress at the last one.
type TransientStops = std::collections::BTreeMap<JobIdentity, (u32, ProgressMark)>;

static TRANSIENT_STOPS: std::sync::Mutex<TransientStops> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn transient_stops() -> std::sync::MutexGuard<'static, TransientStops> {
    TRANSIENT_STOPS.lock().unwrap_or_else(|e| e.into_inner())
}

/// May a transient stop of the job whose row is now `row` send the job
/// back to the queue? Yes for the first [`TRANSIENT_RETRIES`] stops
/// without progress in between: a backend that times out now and then
/// must not end a long job, and one that never answers must not keep it
/// (and its write gate) forever.
pub(crate) fn transient_retry_left(row: &MaintenanceJob) -> bool {
    match transient_stops().get(&identity(row)) {
        Some((n, mark)) if *mark == progress_mark(row) => *n < TRANSIENT_RETRIES,
        _ => true,
    }
}

/// Count a transient stop of the job whose row is now `row`; returns the
/// stops since its last progress.
fn note_transient_stop(row: &MaintenanceJob) -> u32 {
    let mark = progress_mark(row);
    let mut stops = transient_stops();
    let entry = stops.entry(identity(row)).or_insert((0, mark.clone()));
    if entry.1 != mark {
        *entry = (0, mark);
    }
    entry.0 += 1;
    entry.0
}

fn forget_transient_stops(row: &MaintenanceJob) {
    transient_stops().remove(&identity(row));
}

/// Record `n` transient stops of `row` without progress (tests).
#[cfg(test)]
pub(crate) fn set_transient_stops(row: &MaintenanceJob, n: u32) {
    transient_stops().insert(identity(row), (n, progress_mark(row)));
}

/// The backend-health check of `job`: its bucket's backend, and for a
/// migrate the source and target backends by name.
fn backend_check(
    config: &SharedConfig,
    state: &Arc<AppState>,
    job: &MaintenanceJob,
) -> super::gate::BackendCheck {
    let migrate = (job.kind == "migrate")
        .then(|| {
            job.params
                .as_deref()
                .and_then(|p| super::migrate::parse_params(p).ok())
        })
        .flatten();
    let backends = migrate
        .as_ref()
        .map(|p| vec![p.from_backend.clone(), p.target_backend.clone()])
        .unwrap_or_default();
    // A migrate writes its copies through the staging route (the target
    // storage): the encryption witness is asked about it too.
    let mut buckets = vec![job.bucket.clone()];
    let is_migrate = job.kind == "migrate";
    buckets.extend(migrate.map(|p| p.transient_key));
    let check = super::gate::BackendCheck::new(state.clone(), config.clone(), buckets, backends);
    if is_migrate {
        check.ignoring_read_only()
    } else {
        check
    }
}

/// Pure: whether a re-encrypt run writes conditionally (`Ok(true)`) or
/// behind the write gate (`Ok(false)`), after it tried to save a changed
/// gate decision (`now_conditional`) in the job row (`saved`). The gate
/// follows the ROW (`maintenance_gate_arms`). A save that failed while
/// switching to conditional leaves the row gated: the run goes on gated.
/// One that failed while switching to GATED leaves the row ungated, so
/// no gate arms: the run must stop (retried), never write unconditionally
/// while clients write.
fn settle_gate_switch(now_conditional: bool, saved: Result<(), String>) -> Result<bool, String> {
    match saved {
        Ok(()) => Ok(now_conditional),
        Err(_) if now_conditional => Ok(false),
        Err(e) => Err(format!(
            "the switch to the bucket's write gate was not saved ({e}); the run stops and \
             is retried"
        )),
    }
}

/// The three phases, resumable at page granularity. Returns Err(reason)
/// only for job-fatal conditions (per-object failures are recorded and
/// skipped over instead).
async fn execute_phases(
    config: &SharedConfig,
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
) -> Result<(), PhaseStop> {
    let bucket = &job.bucket;

    // ── The write gate of this run: none when every rewrite is
    // conditional on what the job read (judged again now: a backend
    // definition can change while a job waits). ──
    let mut params = super::conversion::ReencryptParams::of(job.params.as_deref());
    // ── Encryption needs atomic writes (N2): a run that encrypts with a
    // proxy key starts only on a backend with a CAS verdict for its
    // current definition, else after a fresh probe that proves them (a
    // queued job of an older release, a backend redefined meanwhile). ──
    {
        let cfg = config.read().await.clone();
        if matches!(
            resolve_desired(&cfg, bucket),
            Ok(super::DesiredEncryption::Proxy { .. })
        ) {
            let what = format!("the re-encrypt job of bucket '{bucket}'");
            if let Err(r) =
                crate::coordination::atomicity::proven_for_bucket(state, &cfg, bucket, &what).await
            {
                return Err(match r.verdict {
                    crate::coordination::CapabilityVerdict::Unknown { .. } => {
                        PhaseStop::FailedTransient(r.message())
                    }
                    _ => PhaseStop::Failed(r.message()),
                });
            }
        }
    }
    let conditional = {
        let cfg = config.read().await.clone();
        super::conversion::rewrites_are_conditional(state, &cfg, bucket).await
    };
    if conditional != params.conditional_writes {
        params.conditional_writes = conditional;
        let db = db.lock().await;
        let saved = db
            .maintenance_set_params(job.id, params.to_json().as_deref())
            .map_err(|e| e.to_string());
        if let Err(e) = &saved {
            warn!("maintenance: job #{} params not saved: {e}", job.id);
        }
        state.maintenance_gate.sync_from(&db);
        params.conditional_writes =
            settle_gate_switch(conditional, saved).map_err(PhaseStop::FailedTransient)?;
    }
    let unconditional = if params.conditional_writes {
        crate::deltaglider::Unconditional::Refused
    } else {
        // ── Drain in-flight writes admitted before the gate armed. ──
        drain_inflight_writes(state, job.id, bucket).await?;
        crate::deltaglider::Unconditional::Allowed
    };
    info!(
        "maintenance: job #{} rewrites '{bucket}' {}",
        job.id,
        if params.conditional_writes {
            "with conditional writes; client writes go on during the run"
        } else {
            "behind the bucket's write gate (no proven conditional writes)"
        }
    );

    // ── The conversion marker of this job's run: a resume extends its
    // deadline (a job without a run never writes a marker). ──
    let marker = super::conversion::begin(&state.engine.load(), bucket, params.run)
        .await
        .map_err(PhaseStop::FailedTransient)?;
    let adoptions = super::conversion::Adoptions::default();

    let mut visitor = Reencrypt {
        config,
        bucket,
        desired: None,
        writes_plaintext: false,
        marker: &marker,
        adoptions: &adoptions,
        unconditional,
    };
    let walked = run_count_then_walk(db, state, holder, job, "rewrite", &mut visitor).await?;
    let mut c = walked.c;
    let mut phase = job.phase.clone();
    if phase == "counting" || phase == "objects" {
        phase = "references".to_string();
        persist(db, job, &phase, &c, None).await;
    }

    // ── Phase: references (deltaspace reference.bin blobs) ──
    if phase == "references" {
        check_cancel(db, job.id).await?;
        let (desired, writes_plaintext) = {
            let cfg = config.read().await;
            let desired = resolve_desired(&cfg, bucket)
                .map_err(|e| format!("config changed mid-run: {e}"))?;
            (desired, super::bucket_writes_plaintext(&cfg, bucket))
        };
        let engine = state.engine.load().clone();
        // The objects walk read them (one listing of the bucket, not two);
        // a resumed job lists them.
        let deltaspaces: Vec<String> = match walked.references {
            Some(seen) => seen.into_iter().collect(),
            None => engine
                .storage()
                .list_reference_prefixes(bucket, "")
                .await
                .map_err(|e| format!("list references failed: {e}"))?,
        };
        for prefix in deltaspaces {
            // A graceful stop waits for the end of the current object only.
            stop_if_shutting_down()?;
            check_cancel(db, job.id).await?;
            let key = format!("{prefix}/.dg/reference.bin");
            let adopted = |meta: &crate::types::FileMetadata| {
                super::conversion::adopted(&marker, writes_plaintext, &key, meta)
            };
            let rewrite = rewrite_reference_if_needed(
                &engine,
                (bucket, &prefix),
                &desired,
                adopted,
                unconditional,
            );
            match rewrite.await {
                Ok(ReferenceRewrite::Kept | ReferenceRewrite::Done { adopted: false }) => {}
                Ok(ReferenceRewrite::Done { adopted: true }) => {
                    adoptions.add();
                    record_adoption(db, job, &key, "converted").await?;
                }
                // A client re-wrote (or a delete reclaimed) the reference
                // after the job read it: it stays as it is now.
                Ok(ReferenceRewrite::Changed) => record_changed(db, job, &key).await?,
                Err(e) => {
                    let reason = format!("{}: {e}", super::conversion::REWRITE_FAILED);
                    record_failure(db, job.id, &key, &reason).await?;
                    c.failed += 1;
                }
            }
            heartbeat(db, job.id, holder).await?;
        }
        // Final counters (the per-reference failure increments above).
        persist(db, job, "references", &c, None).await;
    }

    // ── Phase: purge-versions (optional; before the close) ──
    let mut notes = Vec::new();
    if params.purge_plaintext_versions {
        if phase == "references" {
            phase = super::conversion::PURGE_PHASE.to_string();
            persist(db, job, &phase, &c, None).await;
        }
        let row = current_row(db, job).await;
        if row.objects_failed > 0 {
            notes.push(format!(
                "the noncurrent plaintext versions were not deleted, because {} object(s) were \
                 not converted: a run that converts every object deletes them",
                row.objects_failed
            ));
        } else {
            let deleted =
                purge_plaintext_versions(state, db, holder, job, &Counters::of(&row)).await?;
            notes.push(format!("{deleted} noncurrent plaintext version(s) deleted"));
        }
    }

    // ── Close the conversion: only when nothing failed, by identity. ──
    let row = current_row(db, job).await;
    // Every adoption of the job, a resumed run's included (the row counts
    // them; the in-memory count is this process's run only).
    let adopted = row.objects_adopted.max(adoptions.count());
    let closed = super::conversion::finish(
        &state.engine.load(),
        bucket,
        &marker,
        row.objects_failed,
        adopted,
    )
    .await;
    if let super::conversion::Closed::Blocked(why) = &closed {
        // Never a silent success: the run settles with an error.
        record_failure(db, job.id, super::conversion::MARKER_KEY, why).await?;
        let mut c = Counters::of(&row);
        c.failed += 1;
        persist(db, job, &row.phase, &c, None).await;
    }
    if adopted > 0 {
        notes.insert(
            0,
            format!(
                "{adopted} object(s) adopted after the conversion's cutoff: clients could not \
                 read them, the job converted them (the Failures tab lists up to {} of them)",
                super::store::ADOPTED_SAMPLES
            ),
        );
    }
    if row.objects_changed > 0 {
        notes.push(format!(
            "{} object(s) skipped as changed: a client wrote them after the job read them, \
             and they keep the client's version",
            row.objects_changed
        ));
    }
    if !notes.is_empty() {
        // The run summary names the adoptions and the purge (the generic
        // settle that follows leaves a settled row alone).
        let row = current_row(db, job).await;
        let (status, summary) =
            super::settle_status(row.objects_done, row.objects_skipped, row.objects_failed);
        notes.extend(summary);
        let note = notes.join("; ");
        let db = db.lock().await;
        if let Err(e) = db.maintenance_finish(job.id, status, Some(&note)) {
            warn!("maintenance: failed to settle job #{}: {}", job.id, e);
        }
    }

    Ok(())
}

/// The purge phase: delete the noncurrent plaintext versions of the
/// bucket's encrypted keys, one backend page at a time (8 requests at once
/// inside a page), the cursor and the count saved after each page. A
/// bucket without versions is a no-op. Returns the versions deleted in
/// this run, a resumed phase included.
async fn purge_plaintext_versions(
    state: &Arc<AppState>,
    db: &Arc<Mutex<JobsDb>>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
    c: &Counters,
) -> Result<u64, PhaseStop> {
    use super::conversion::{PurgeCursor, PURGE_PHASE};
    let engine = state.engine.load().clone();
    let storage = engine.storage();
    // A read error stops the phase (retried): a purge that the job was
    // asked for never ends as "not versioned" by mistake.
    let versioned = storage
        .bucket_versioning(&job.bucket)
        .await
        .map_err(|e| PhaseStop::FailedTransient(format!("the purge could not start: {e}")))?;
    if !versioned {
        return Ok(0);
    }
    let mut cursor = PurgeCursor::parse(resumable_in(job, PURGE_PHASE).as_deref());
    loop {
        stop_if_shutting_down()?;
        check_cancel(db, job.id).await?;
        let page = storage
            .purge_plaintext_versions(&job.bucket, cursor.at.as_deref())
            .await
            .map_err(|e| {
                PhaseStop::FailedTransient(format!(
                    "the purge of noncurrent plaintext versions failed: {e}"
                ))
            })?;
        cursor.deleted += page.deleted;
        if page.deleted > 0 {
            super::conversion::audit(
                "encryption_conversion_versions_purged",
                "system",
                &format!(
                    "job:{}: {} noncurrent plaintext version(s) deleted",
                    job.id, page.deleted
                ),
                &job.bucket,
            );
        }
        let Some(next) = page.next else {
            break;
        };
        cursor.at = Some(next);
        persist(db, job, PURGE_PHASE, c, Some(&cursor.token())).await;
        heartbeat(db, job.id, holder).await?;
    }
    Ok(cursor.deleted)
}

/// The reencrypt per-object step: rewrite an object whose at-rest state
/// does not match the backend's current mode.
struct Reencrypt<'a> {
    config: &'a SharedConfig,
    bucket: &'a str,
    desired: Option<DesiredEncryption>,
    /// The bucket writes plaintext (its rule serves plaintext): its
    /// unsealed objects are never adopted.
    writes_plaintext: bool,
    /// The bucket's conversion marker for this run.
    marker: &'a super::conversion::JobMarker,
    adoptions: &'a super::conversion::Adoptions,
    /// Whether a rewrite may be unconditional on a backend that answers
    /// 501 to a condition: only behind the bucket's write gate.
    unconditional: crate::deltaglider::Unconditional,
}

impl ObjectVisitor for Reencrypt<'_> {
    const CONCURRENCY: usize = REENCRYPT_CONCURRENCY;

    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        // Re-resolve the desired state every page: a config apply mid-run
        // swaps the engine; the job must follow (or abort if the mode
        // became unsupported).
        let cfg = self.config.read().await;
        self.desired = Some(
            resolve_desired(&cfg, self.bucket)
                .map_err(|e| format!("config changed mid-run: {e}"))?,
        );
        self.writes_plaintext = super::bucket_writes_plaintext(&cfg, self.bucket);
        Ok(())
    }

    async fn visit(
        &self,
        engine: &Arc<crate::deltaglider::DynEngine>,
        key: &str,
        listed: Option<crate::storage::ObjectVariant>,
    ) -> Visit {
        let desired = self.desired.as_ref().expect("begin_page runs first");
        let failed = |e: &dyn std::fmt::Display| {
            Visit::Failed(format!("{}: {e}", super::conversion::REWRITE_FAILED))
        };
        // A key that is gone since the listing: a client deleted it (or
        // replaced it in a way the listing did not see). Nothing to do.
        let (meta, forms) = match engine.head_listed(self.bucket, key, listed).await {
            Ok(read) => read,
            Err(crate::deltaglider::EngineError::NotFound(_)) => return Visit::Changed,
            Err(e) => return failed(&format!("could not read object metadata: {e}")),
        };
        // The newest stored form (the object), and the other one when a
        // crash between a write and its other-form DELETE left both: each
        // is rewritten in place, so no plaintext form stays behind.
        let newest = form_of(&meta);
        let mut targets = vec![(newest, meta)];
        let other = match newest {
            ObjectVariant::Delta => ObjectVariant::Passthrough,
            ObjectVariant::Passthrough => ObjectVariant::Delta,
        };
        if forms.has_form(other) {
            match engine.stored_form(self.bucket, key, other).await {
                Ok(Some(m)) => targets.push((other, m)),
                Ok(None) => {}
                Err(e) => return failed(&format!("could not read object metadata: {e}")),
            }
        }
        let mut bytes = 0i64;
        let mut wrote = false;
        let mut changed = false;
        for (i, (form, read)) in targets.iter().enumerate() {
            if !needs_rewrite(&read.user_metadata, desired) {
                continue;
            }
            let rewrite = engine.rewrite_object_if_unchanged(
                self.bucket,
                key,
                *form,
                read,
                self.unconditional,
            );
            match rewrite.await {
                // The count and the bytes are the object's (the newest form).
                Ok(Rewritten::Done { bytes: b }) if i == 0 => {
                    wrote = true;
                    bytes = b as i64;
                }
                Ok(Rewritten::Done { .. }) => {}
                // The object is a client's newer write now, sealed by the
                // proxy that took it: it stays as the client wrote it.
                Ok(Rewritten::Changed) if i == 0 => changed = true,
                // A stale other form that a client write replaced.
                Ok(Rewritten::Changed) => {}
                Err(e) => return failed(&e),
            }
        }
        if changed {
            return Visit::Changed;
        }
        if !wrote {
            return Visit::Skipped;
        }
        // Judged on this read (the backend's metadata, never a cache).
        if super::conversion::adopted(self.marker, self.writes_plaintext, key, &targets[0].1) {
            self.adoptions.add();
            return Visit::Adopted { bytes };
        }
        Visit::Done { bytes }
    }
}

/// The stored form whose metadata `meta` is.
fn form_of(meta: &crate::types::FileMetadata) -> ObjectVariant {
    match meta.storage_info {
        crate::types::StorageInfo::Delta { .. } => ObjectVariant::Delta,
        _ => ObjectVariant::Passthrough,
    }
}

/// What one object of an `objects` phase ended in.
pub(crate) enum Visit {
    Done {
        bytes: i64,
    },
    /// Converted, although clients could not read it (stored after the
    /// conversion's cutoff): counted as done, and listed in the failure
    /// ring as information.
    Adopted {
        bytes: i64,
    },
    Skipped,
    /// Not rewritten: a client wrote (or deleted) the object after the job
    /// read it, and it stays as the client left it. Counted as skipped and
    /// as changed (`skipped_changed`); never a failure.
    Changed,
    /// Recorded in the failure ring and counted; the phase goes on.
    Failed(String),
}

/// The per-object work of a count-then-walk job kind (reencrypt,
/// backfill-metadata). [`run_count_then_walk`] owns the rest.
pub(crate) trait ObjectVisitor {
    /// Objects of a page in flight at once.
    const CONCURRENCY: usize = PAGE_CONCURRENCY;
    /// Runs before each listing page.
    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        Ok(())
    }
    /// One user object, and the one stored form of it that the page's
    /// listing proved (`None`: not proved). The visitor reads the object
    /// ([`read_listed`], or a read that its write reuses). Up to
    /// [`ObjectVisitor::CONCURRENCY`] objects run at once.
    async fn visit(
        &self,
        engine: &Arc<crate::deltaglider::DynEngine>,
        key: &str,
        listed: Option<crate::storage::ObjectVariant>,
    ) -> Visit;
}

/// One HEAD of the stored form of `key` that the listing proved (the write
/// gate freezes the bucket), not one per form; both forms when `listed` is
/// `None`. Its error is the object's failed visit.
pub(crate) async fn read_listed(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    listed: Option<crate::storage::ObjectVariant>,
) -> Result<(crate::types::FileMetadata, crate::deltaglider::StoredForms), Visit> {
    engine
        .head_listed(bucket, key, listed)
        .await
        .map_err(|e| Visit::Failed(format!("could not read object metadata: {e}")))
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

/// What [`run_count_then_walk`] ends with.
pub(crate) struct Walked {
    /// The counters at the end of `objects` (the row's, for a job
    /// persisted in a later phase).
    pub c: Counters,
    /// The deltaspaces whose reference.bin the walk read, when it read
    /// every page from the first one and each page reported them; `None`:
    /// list them.
    pub references: Option<std::collections::BTreeSet<String>>,
}

/// The `objects` phase that the reencrypt and backfill kinds share, on
/// [`paged_phase`]: per-object failures recorded and skipped over. One
/// listing of the bucket: the total comes from the bucket's usage counter
/// when a full scan stamped it ([`trusted_object_count`]), else it stays
/// unknown (the bar is indeterminate) until the walk ends; the walk ends
/// with the exact total, and it collects the deltaspaces whose
/// reference.bin it read. A job persisted in a later phase gets its row's
/// counters back untouched. `what` names the job in the page-budget error.
pub(crate) async fn run_count_then_walk<V: ObjectVisitor>(
    db: &Arc<Mutex<JobsDb>>,
    state: &Arc<AppState>,
    holder: Holder<'_>,
    job: &MaintenanceJob,
    what: &str,
    visitor: &mut V,
) -> Result<Walked, PhaseStop> {
    let mut phase = job.phase.clone();
    let mut c = Counters::of(job);

    // ── Phase: counting ── (the initial phase of a job; a job that a
    // previous release persisted mid-count drops its partial count)
    if phase == "counting" {
        c = Counters {
            total: trusted_object_count(state, &job.bucket),
            ..Counters::default()
        };
        phase = "objects".to_string();
        persist(db, job, &phase, &c, None).await;
    }

    // ── Phase: objects ──
    if phase != "objects" {
        return Ok(Walked {
            c,
            references: None,
        });
    }
    let budget = format!(
        "{what} stopped at the page budget with more pages pending — bucket \
         too large for one pass; job left resumable in phase 'objects' \
         (cursor persisted)"
    );
    // Resume only when the job was persisted IN this phase (a fresh
    // transition from counting starts at page 0).
    let resume = resumable_in(job, "objects");
    // No trusted total: count the bucket next to the walk.
    let count = c
        .total
        .is_none()
        .then(|| BackgroundCount::spawn(state.engine.load().clone(), job.bucket.clone()));
    let mut step = ObjectsStep {
        db,
        state,
        job,
        visitor,
        engine: None,
        forms: Default::default(),
        // A resumed walk misses the pages before its cursor.
        references: resume.is_none().then(Default::default),
        c,
        count,
    };
    paged_phase(
        JobCtx { db, holder, job },
        PhaseSpec {
            phase: "objects",
            resume,
            // Progress and a cancel every few objects, not per 1000: a
            // page of large objects can take minutes.
            cancel_every: Some(CANCEL_CHECK_EVERY),
            budget_exhausted: &budget,
            concurrency: V::CONCURRENCY,
        },
        &mut step,
    )
    .await?;
    let mut c = step.c;
    // Every listed object ended done, skipped or failed: the exact total.
    c.total = Some(c.done + c.skipped + c.failed);
    persist(db, job, "objects", &c, None).await;
    Ok(Walked {
        c,
        references: step.references,
    })
}

/// The object count of `bucket` from the usage counter, when a full scan
/// stamped its row (the quota gate's trust rule): the job's progress
/// total without a counting listing. `None`: unknown until the walk ends.
pub(crate) fn trusted_object_count(state: &AppState, bucket: &str) -> Option<i64> {
    let row = state.bucket_usage.as_ref()?.read(bucket).ok().flatten()?;
    row.last_scan_at?;
    i64::try_from(row.object_count).ok()
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
    db: &'a Arc<Mutex<JobsDb>>,
    state: &'a Arc<AppState>,
    job: &'a MaintenanceJob,
    visitor: &'a mut V,
    /// The page's engine (one per page: an apply mid-page swaps it).
    engine: Option<Arc<crate::deltaglider::DynEngine>>,
    /// The stored forms that the page's listing proved.
    forms: crate::deltaglider::ListedPageForms,
    /// The deltaspaces whose reference.bin the pages read so far; `None`
    /// once the walk cannot vouch for all of them ([`Walked::references`]).
    references: Option<std::collections::BTreeSet<String>>,
    c: Counters,
    /// The bucket's object count, made next to the walk (`None`: the total
    /// was known at the start).
    count: Option<BackgroundCount>,
}

impl<V: ObjectVisitor> ObjectsStep<'_, V> {
    /// Save the counters with `token`, the total from the count once known.
    async fn save_at(&mut self, token: Option<&str>) {
        if self.c.total.is_none() {
            self.c.total = self.count.as_ref().and_then(BackgroundCount::get);
        }
        persist(self.db, self.job, "objects", &self.c, token).await;
    }
}

impl<V: ObjectVisitor> PageStep for ObjectsStep<'_, V> {
    type Done = Visit;

    async fn begin_page(&mut self) -> Result<(), PhaseStop> {
        self.visitor.begin_page().await?;
        self.engine = Some(self.state.engine.load().clone());
        Ok(())
    }

    async fn list(&mut self, token: Option<&str>) -> Result<KeyPage, ListError> {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        let (page, forms, references) =
            list_job_keys(engine, &self.job.bucket, token, "object list failed").await?;
        self.forms = forms;
        match (self.references.as_mut(), references) {
            (Some(seen), Some(page_refs)) => seen.extend(page_refs),
            // A page that does not report them: list them at the end.
            (_, None) => self.references = None,
            (None, Some(_)) => {}
        }
        Ok(page)
    }

    /// The walk starts again at page 0: it reads every reference again.
    fn on_restart(&mut self) {
        self.references = Some(Default::default());
    }

    // A restart re-scans from page 0: the visitor skips objects already
    // done, so the re-scan is idempotent. Counters are NOT reset:
    // re-encountered objects tally as `skipped` again — display drift only,
    // never a second rewrite.

    async fn save(&mut self, token: Option<&str>) -> Result<(), PhaseStop> {
        self.save_at(token).await;
        Ok(())
    }

    /// Mid-page: the counters so far, with the cursor of the page in
    /// progress (a resume redoes the page; done objects then skip).
    async fn mid_page(&mut self, page_token: Option<&str>) -> Result<(), PhaseStop> {
        self.save_at(page_token).await;
        Ok(())
    }

    async fn object(&self, key: &str) -> Visit {
        let engine = self.engine.as_ref().expect("begin_page runs first");
        self.visitor.visit(engine, key, self.forms.of(key)).await
    }

    async fn settle(&mut self, key: &str, visit: Visit) -> Result<(), PhaseStop> {
        match visit {
            Visit::Done { bytes } => {
                self.c.done += 1;
                self.c.bytes += bytes;
            }
            Visit::Adopted { bytes } => {
                self.c.done += 1;
                self.c.bytes += bytes;
                record_adoption(self.db, self.job, key, "converted").await?;
            }
            Visit::Skipped => self.c.skipped += 1,
            Visit::Changed => {
                self.c.skipped += 1;
                record_changed(self.db, self.job, key).await?;
            }
            Visit::Failed(reason) => {
                record_failure(self.db, self.job.id, key, &reason).await?;
                self.c.failed += 1;
            }
        }
        Ok(())
    }
}

/// Re-store a deltaspace's reference blob through the (encrypting)
/// storage wrapper when its at-rest state doesn't match. The reference's
/// PLAINTEXT bytes are unchanged, so the engine's in-memory
/// ReferenceCache (keyed by content) stays valid. The check and the
/// rewrite run under the deltaspace's prefix lock AND the cross-instance
/// reference lock (`with_dest_prefix_lock`), like every reference write,
/// and the write is conditional on the version that the job read
/// ([`crate::deltaglider::DynEngine::rewrite_reference_if_unchanged`]).
async fn rewrite_reference_if_needed(
    engine: &crate::deltaglider::DynEngine,
    (bucket, prefix): (&str, &str),
    desired: &DesiredEncryption,
    adopted: impl Fn(&crate::types::FileMetadata) -> bool,
    unconditional: crate::deltaglider::Unconditional,
) -> Result<ReferenceRewrite, String> {
    let res = engine
        .with_dest_prefix_lock(bucket, prefix, || {
            engine.rewrite_reference_if_unchanged(
                bucket,
                prefix,
                |meta| needs_rewrite(&meta.user_metadata, desired),
                unconditional,
            )
        })
        .await;
    match res {
        Ok(None) => Ok(ReferenceRewrite::Kept),
        Ok(Some((Rewritten::Changed, _))) => Ok(ReferenceRewrite::Changed),
        Ok(Some((Rewritten::Done { .. }, read))) => Ok(ReferenceRewrite::Done {
            adopted: adopted(&read),
        }),
        Err(e) => Err(format!(
            "reference rewrite failed for {bucket}/{prefix}: {e}"
        )),
    }
}

/// What the references phase did with one reference.bin.
enum ReferenceRewrite {
    /// None there, or it is in the desired state already.
    Kept,
    /// Rewritten; `adopted`: the job's read of it was adopted (a client
    /// could not read it: it is reported like an object).
    Done { adopted: bool },
    /// A client re-wrote or reclaimed it after the job read it.
    Changed,
}

/// Count one object (or reference) `key` that the job did not rewrite
/// because a client wrote it after the job read it.
async fn record_changed(
    db: &Arc<Mutex<JobsDb>>,
    job: &MaintenanceJob,
    key: &str,
) -> Result<(), PhaseStop> {
    stop_if_shutting_down()?;
    if let Err(e) = db.lock().await.maintenance_count_changed(job.id) {
        warn!("maintenance: changed count failed for job #{}: {e}", job.id);
    }
    tracing::debug!(
        "maintenance: job #{} left {}/{key} as a client wrote it after the job read it",
        job.id,
        job.bucket
    );
    Ok(())
}

/// Wait for the gated bucket's in-flight writes to reach zero: the S3
/// writes admitted before the gate armed, and the writes that run on
/// without a request (background writers: replication copies and mirror
/// deletes, lifecycle acts, `admit_all`; detached request work: DeleteObjects
/// batches and CompleteMultipartUpload stores, `begin_detached_write`). The
/// wait is bounded by the server's request timeout. A request that is
/// still open cannot outlive it, so a drain that times out on request
/// writes alone fails the job. The writes that run on without a request
/// have no request deadline (a replication copy is bounded by its object
/// timeout, 30 min by default): a drain that times out while one is in
/// flight DEFERS the job (`PhaseStop::Deferred`: the row goes back to
/// `queued`, the gate stays armed, the drain runs again later) and warns
/// once, naming the job, the bucket and the writes in flight.
pub(crate) async fn drain_inflight_writes(
    state: &Arc<AppState>,
    job_id: i64,
    bucket: &str,
) -> Result<(), PhaseStop> {
    let ceiling = std::time::Duration::from_secs(state.engine.load().tuning().request_timeout_secs);
    let drained = drain_inflight_writes_within(&state.maintenance_gate, bucket, ceiling).await;
    if let Err(PhaseStop::Deferred(why)) = &drained {
        warn!("maintenance: job #{job_id} deferred: {why}; it retries once they end");
    }
    drained
}

/// [`drain_inflight_writes`] with its ceiling.
pub(crate) async fn drain_inflight_writes_within(
    gate: &crate::maintenance::gate::MaintenanceGate,
    bucket: &str,
    ceiling: std::time::Duration,
) -> Result<(), PhaseStop> {
    let deadline = std::time::Instant::now() + ceiling;
    while gate.inflight_writes(bucket) > 0 {
        if std::time::Instant::now() > deadline {
            // Read the background count, then the total once more: a write
            // that ended between the two reads is no reason to fail.
            let background = gate.inflight_background_writes(bucket);
            let total = gate.inflight_writes(bucket);
            return match drain_timeout_stop(bucket, ceiling, total, background) {
                Some(stop) => Err(stop),
                None => Ok(()),
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(DRAIN_POLL_MS)).await;
    }
    Ok(())
}

/// Pure: how a drain that timed out stops its job, from the in-flight
/// counts read after the deadline (the background count first, then the
/// total). Nothing in flight on the re-read: no stop (`None`). Background
/// or detached writes in flight: deferred (they end on their own, the job
/// retries later). Request writes only: failed (no open request outlives
/// the request timeout).
pub(crate) fn drain_timeout_stop(
    bucket: &str,
    ceiling: std::time::Duration,
    total_inflight: i64,
    background_inflight: i64,
) -> Option<PhaseStop> {
    if total_inflight <= 0 {
        None
    } else if background_inflight > 0 {
        Some(PhaseStop::Deferred(format!(
            "{background_inflight} background write(s) to '{bucket}' (replication, lifecycle, \
             detached DeleteObjects or CompleteMultipartUpload) still in flight after {}s",
            ceiling.as_secs()
        )))
    } else {
        Some(PhaseStop::Failed(format!(
            "in-flight writes to '{bucket}' did not drain within {}s",
            ceiling.as_secs()
        )))
    }
}

/// Cancellation check between pages. Maps "operator asked to cancel"
/// into the Err channel so phases unwind; the caller distinguishes it.
pub(crate) async fn check_cancel(db: &Arc<Mutex<JobsDb>>, job_id: i64) -> Result<(), PhaseStop> {
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
    /// A backend of the job is gated (unreachable, or its credentials are
    /// refused). The row goes back to `queued` with its cursor, and the job
    /// waits before a worker claims it again.
    Deferred(String),
    /// A job-fatal error.
    Failed(String),
    /// A job-fatal error that a backend that did not answer caused (a
    /// timeout, a 5xx, a throttle) after the in-place retries. The job
    /// settles `failed`, but a migrate keeps the copies it made.
    FailedTransient(String),
}

impl std::fmt::Display for PhaseStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PhaseStop::Cancelled => f.write_str("cancelled"),
            PhaseStop::LeaseLost => f.write_str("lease lost"),
            PhaseStop::ShuttingDown => f.write_str("shutting down"),
            PhaseStop::Deferred(why) => write!(f, "deferred: {why}"),
            PhaseStop::Failed(e) | PhaseStop::FailedTransient(e) => f.write_str(e),
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
    /// A backend of the job is gated, or a transient failure stopped it
    /// with retries left: hand the row back, claimable later.
    Defer,
}

/// The decision for [`AfterRun`]. Any error while the process shuts down
/// counts as the shutdown: the runtime teardown makes healthy work fail
/// (a refused `spawn_blocking`), and settling on it leaves a half-done
/// bucket marked finished. An operator cancel stays a cancel.
pub(crate) fn after_run(
    outcome: &Result<(), PhaseStop>,
    shutting_down: bool,
    retry_transient: bool,
) -> AfterRun {
    match outcome {
        Ok(()) => AfterRun::Settle,
        Err(PhaseStop::LeaseLost) => AfterRun::LeaveToLeaseHolder,
        Err(PhaseStop::ShuttingDown) => AfterRun::ReleaseForResume,
        Err(PhaseStop::Cancelled) => AfterRun::Settle,
        Err(PhaseStop::Deferred(_)) => AfterRun::Defer,
        Err(PhaseStop::Failed(_) | PhaseStop::FailedTransient(_)) if shutting_down => {
            AfterRun::ReleaseForResume
        }
        Err(PhaseStop::FailedTransient(_)) if retry_transient => AfterRun::Defer,
        Err(PhaseStop::Failed(_) | PhaseStop::FailedTransient(_)) => AfterRun::Settle,
    }
}

/// Save the job's progress. The migrate phases save through
/// [`persist_flushed`] instead, which takes the proof that their deferred
/// copies are durable; a delete is durable per key.
pub(crate) async fn persist(
    db: &Arc<Mutex<JobsDb>>,
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
///
/// The commit is `synchronous = FULL`. The flip and the source delete come
/// after a migrate phase save: a power loss that reverts the row to an
/// earlier phase, with the YAML already flipped, would let a cancel or a
/// failure of the resumed job unwind (delete) the live data on the target.
/// One fsync per page or phase costs nothing next to the page's copies.
pub(crate) async fn persist_flushed(
    _proof: super::migrate::Flushed,
    db: &Arc<Mutex<JobsDb>>,
    job: &MaintenanceJob,
    phase: &str,
    c: &Counters,
    token: Option<&str>,
) {
    let db = db.lock().await;
    if let Err(e) = db.maintenance_update_progress_durable(
        job.id, phase, c.total, c.done, c.skipped, c.failed, c.bytes, token,
    ) {
        warn!(
            "maintenance: durable progress persist failed for job #{}: {}",
            job.id, e
        );
    }
}

/// The job's lease keeper. The per-page `heartbeat` is not enough on its
/// own: one page of copy work can outlast the TTL, and a lapsed lease lets
/// the requeue scan hand the job back and the write gate open mid-job.
/// When the keeper gives the lease up, the next per-page `heartbeat`
/// reports `LeaseLost` (a lapsed lease never renews).
pub(crate) fn spawn_lease_keeper(
    db: Arc<Mutex<JobsDb>>,
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
    db: &Arc<Mutex<JobsDb>>,
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
    db: &Arc<Mutex<JobsDb>>,
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

/// Report one object (or reference) `key` that the conversion job
/// converted (or the migrate job encrypted on its target) although a
/// client could not read it: counted in the job row, sampled in the
/// failure ring, audited. `did`: what the job did with it.
pub(crate) async fn record_adoption(
    db: &Arc<Mutex<JobsDb>>,
    job: &MaintenanceJob,
    key: &str,
    did: &str,
) -> Result<(), PhaseStop> {
    stop_if_shutting_down()?;
    let note = format!(
        "{}: {did}, but clients could not read it (no conversion window of this job's run, \
         or the backend stored it after the conversion's cutoff, or names no time)",
        super::conversion::ADOPTED_AFTER_CUTOFF
    );
    if let Err(e) = db
        .lock()
        .await
        .maintenance_record_adoption(job.id, key, &note)
    {
        warn!(
            "maintenance: adoption record failed for job #{}: {}",
            job.id, e
        );
    }
    super::conversion::audit_with(
        "encryption_conversion_adopted_after_cutoff",
        "system",
        &format!("job:{}", job.id),
        &job.bucket,
        key,
        None,
    );
    Ok(())
}

#[cfg(test)]
#[path = "worker_cost_tests.rs"]
pub(crate) mod cost_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// The drain's stop after its deadline, from the counts re-read then
    /// (background first, then the total; the guard lowers them in the
    /// other order). A write that ended between the reads fails nothing.
    #[test]
    fn drain_timeout_stop_truth_table() {
        let c = std::time::Duration::from_secs(1);
        assert!(
            drain_timeout_stop("b", c, 0, 0).is_none(),
            "drained on the re-read"
        );
        assert!(
            drain_timeout_stop("b", c, 0, 1).is_none(),
            "drained on the re-read"
        );
        assert!(matches!(
            drain_timeout_stop("b", c, 2, 1),
            Some(PhaseStop::Deferred(_))
        ));
        assert!(matches!(
            drain_timeout_stop("b", c, 1, 0),
            Some(PhaseStop::Failed(_))
        ));
    }

    /// The background count rises before the total and falls after it, so
    /// a reader that sees a background write in the total sees it in the
    /// background count too.
    #[test]
    fn a_background_guard_moves_its_counts_in_order() {
        let gate = Arc::new(crate::maintenance::gate::MaintenanceGate::new());
        let w = gate.begin_detached_write("b");
        assert_eq!(
            (
                gate.inflight_writes("b"),
                gate.inflight_background_writes("b")
            ),
            (1, 1)
        );
        drop(w);
        assert_eq!(
            (
                gate.inflight_writes("b"),
                gate.inflight_background_writes("b")
            ),
            (0, 0)
        );
    }

    /// Review of #166: background writes are now counted, and a long
    /// replication copy has no request deadline, so a drain that waited
    /// past its ceiling for one FAILED the migrate / re-encrypt / backfill.
    /// It defers now; a drain stuck on client writes alone still fails.
    #[tokio::test]
    async fn a_drain_that_times_out_on_background_writes_defers() {
        let gate = Arc::new(crate::maintenance::gate::MaintenanceGate::new());
        let ceiling = std::time::Duration::from_millis(60);
        let background = crate::maintenance::gate::admit_all(Some(&gate), &["b"]).unwrap();
        let client = gate.begin_write("b");
        let stop = drain_inflight_writes_within(&gate, "b", ceiling)
            .await
            .unwrap_err();
        assert!(matches!(stop, PhaseStop::Deferred(_)), "{stop:?}");
        drop(background);
        assert_eq!(gate.inflight_background_writes("b"), 0);
        let stop = drain_inflight_writes_within(&gate, "b", ceiling)
            .await
            .unwrap_err();
        assert!(matches!(stop, PhaseStop::Failed(_)), "{stop:?}");
        drop(client);
        drain_inflight_writes_within(&gate, "b", ceiling)
            .await
            .expect("nothing in flight drains at once");
        // After a deferral the job's next run drains once the copy ends.
        let w = crate::maintenance::gate::admit_all(Some(&gate), &["b"]).unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            drop(w);
        });
        drain_inflight_writes_within(&gate, "b", std::time::Duration::from_secs(5))
            .await
            .expect("drains once the background write ends");
        release.await.unwrap();
    }
    use crate::storage::DynStorageBackend;

    /// Finding 4: a failed save of a switch to the gate stops the run; it
    /// never continues ungated with unconditional writes allowed.
    #[test]
    fn a_failed_switch_to_the_gate_stops_the_run() {
        assert_eq!(settle_gate_switch(true, Ok(())), Ok(true));
        assert_eq!(settle_gate_switch(false, Ok(())), Ok(false));
        assert_eq!(
            settle_gate_switch(true, Err("disk".into())),
            Ok(false),
            "the row stays gated: run gated"
        );
        assert!(
            settle_gate_switch(false, Err("disk".into())).is_err(),
            "the row stays ungated: no gate arms, so the run stops"
        );
    }

    fn claimed_job(ttl: i64) -> (Arc<Mutex<JobsDb>>, i64) {
        let db = JobsDb::in_memory("testpass").unwrap();
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
    /// Review B3: a job row on the coordination bucket that an older
    /// release accepted resumes after the upgrade; it fails before any
    /// phase reads or rewrites that bucket.
    #[tokio::test]
    async fn a_resumed_job_on_the_coordination_bucket_fails() {
        let data = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            config_sync_bucket: Some("dgp-sync".into()),
            ..Default::default()
        };
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(data.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        );
        engine.create_bucket("dgp-sync").await.unwrap();
        engine
            .store("dgp-sync", "lease.json", b"{}", None, Default::default())
            .await
            .unwrap();
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
            jobs_db: None,
            maintenance_gate: Arc::new(crate::maintenance::gate::MaintenanceGate::new()),
            maintenance_notify: Arc::new(tokio::sync::Notify::new()),
            backend_capabilities: Default::default(),
            backend_health: Default::default(),
            encryption_witness: None,
            config_db_key_mismatch: false,
            coordination: Default::default(),
        });
        let db = JobsDb::in_memory("testpass").unwrap();
        let id = db
            .maintenance_create_job(
                super::super::backfill::KIND,
                "dgp-sync",
                "scanning",
                None,
                "admin",
                1,
            )
            .unwrap()
            .unwrap();
        let job = db
            .maintenance_claim_next_job("inst", current_unix_seconds(), 60)
            .unwrap()
            .unwrap();
        let db = Arc::new(Mutex::new(db));
        let shared: crate::config::SharedConfig = Arc::new(tokio::sync::RwLock::new(config));
        let mutator = crate::config_apply::ConfigMutator {
            config: shared.clone(),
            app: state.clone(),
            persist_path: data.path().join("cfg.yaml").display().to_string(),
        };
        run_job(&mutator, &shared, &db, &state, Holder::test("inst"), job).await;
        let row = db.lock().await.maintenance_job_by_id(id).unwrap().unwrap();
        assert_eq!(row.status, "failed", "{row:?}");
        assert!(
            row.last_error
                .as_deref()
                .unwrap_or("")
                .contains("coordination bucket"),
            "{:?}",
            row.last_error
        );
    }

    #[test]
    fn after_run_never_settles_on_a_shutdown() {
        use AfterRun::*;
        let failed = || Err(PhaseStop::Failed("object list failed: x".into()));
        let transient = || Err(PhaseStop::FailedTransient("list timed out".into()));
        // A transient failure goes back to the queue while it has retries
        // left; then it settles. A shutdown releases it either way.
        for (shutting_down, retry, want) in [
            (false, true, Defer),
            (false, false, Settle),
            (true, true, ReleaseForResume),
            (true, false, ReleaseForResume),
        ] {
            assert_eq!(after_run(&transient(), shutting_down, retry), want);
            assert_eq!(
                after_run(&failed(), shutting_down, retry),
                after_run(&failed(), shutting_down, false),
                "a definitive failure never retries"
            );
        }
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
                after_run(&outcome, shutting_down, false),
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
