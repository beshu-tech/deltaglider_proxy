// SPDX-License-Identifier: BUSL-1.1

//! Lifecycle execution through the DeltaGlider engine.

use super::planner::{
    compile_rule_globs, earlier_due, is_internal_key, lifecycle_prefix, plan_object,
    plan_retain_newest, Candidate, Decision, PlannedLifecycleAction, QualifySpec, SkipReason,
};
use super::state_store::{LifecycleFailureInsert, LifecycleRunTotals};
use crate::background::{LeaseAlive, LeaseKeeper, RunLease};
use crate::config_db::JobsDb;
use crate::config_sections::{LifecycleAction, LifecycleRetainNewestAction, LifecycleRule};
use crate::coordination::LeaseError;
use crate::deltaglider::{DeleteHooks, DeleteItem, DeleteOutcome, DynEngine, ListObjectsPage};
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::job_loop::{list_retry_delay, ListFailure, Pager};
use crate::transfer::{
    copy_object_with_retries, ObjectTransferRequest, TransferProvenance,
    LIFECYCLE_RULE_METADATA_KEY,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewObject {
    pub bucket: String,
    pub key: String,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_key: Option<String>,
    pub delete_source_after_success: bool,
    pub created_at: String,
    /// Logical (uncompressed) size.
    pub size: u64,
    /// Bytes on the backend: a delta is much smaller than `size`.
    pub stored_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LifecycleFailure {
    pub key: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct LifecycleRunOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<i64>,
    pub rule_name: String,
    pub status: String,
    pub objects_scanned: i64,
    pub objects_affected: i64,
    pub objects_skipped: i64,
    pub bytes_affected: i64,
    pub errors: i64,
    /// retain-newest only: candidates excluded by `qualify` (too small / too
    /// young) — never kept, never deleted. Default 0 for age rules.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub objects_ignored: i64,
    /// retain-newest only: candidates spared by `protect_younger_than` this run.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub objects_protected: i64,
    /// Of `objects_skipped`: kept out of the rule by an include/exclude
    /// glob, an internal key or a folder marker.
    #[serde(default)]
    pub objects_excluded: i64,
    /// Of `objects_skipped`: not old enough yet.
    #[serde(default)]
    pub objects_not_due: i64,
    /// Of `objects_affected`: deleted (a preview: would delete).
    #[serde(default)]
    pub objects_deleted: i64,
    /// Of `objects_affected`: transitioned, and the source deleted.
    #[serde(default)]
    pub objects_moved: i64,
    /// Of `objects_affected`: transitioned, and the source kept.
    #[serde(default)]
    pub objects_copied: i64,
    /// Bytes on the backend of the affected objects (`bytes_affected` is
    /// their logical size). Deltas make it much smaller.
    #[serde(default)]
    pub stored_bytes_affected: i64,
    pub candidates: Vec<PreviewObject>,
    pub failures: Vec<LifecycleFailure>,
    /// Writes (copies and deletes) tried and failed so far: a run whose
    /// first writes all fail stops early.
    #[serde(skip)]
    pub(crate) write_attempts: i64,
    #[serde(skip)]
    pub(crate) write_failures: i64,
    /// A preview only: when the walk that answers ended (RFC 3339). A
    /// preview can answer from an earlier walk, so the GUI shows this,
    /// not the time it received the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed_at: Option<String>,
    /// The earliest moment (RFC 3339) at which a scanned object that the
    /// rule does not match now will match: its `created_at` plus
    /// `expire_after`. Absent when no scanned object will ever match, and
    /// for a rule without an age condition (retain-newest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<String>,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

impl LifecycleRunOutcome {
    /// The counters a running run publishes.
    pub fn counters(&self) -> super::live::LiveCounters {
        super::live::LiveCounters {
            objects_scanned: self.objects_scanned,
            objects_affected: self.objects_affected,
            objects_skipped: self.objects_skipped,
            objects_excluded: self.objects_excluded,
            objects_not_due: self.objects_not_due,
            objects_deleted: self.objects_deleted,
            objects_moved: self.objects_moved,
            objects_copied: self.objects_copied,
            bytes_affected: self.bytes_affected,
            errors: self.errors,
        }
    }

    /// The totals of the run-history row.
    fn totals(&self) -> LifecycleRunTotals {
        LifecycleRunTotals {
            objects_scanned: self.objects_scanned,
            objects_affected: self.objects_affected,
            objects_skipped: self.objects_skipped,
            bytes_affected: self.bytes_affected,
            errors: self.errors,
            objects_deleted: self.objects_deleted,
            objects_moved: self.objects_moved,
            objects_copied: self.objects_copied,
            objects_excluded: self.objects_excluded,
            objects_not_due: self.objects_not_due,
        }
    }
}

/// How many writes a run tries before it judges them: when the first
/// [`EARLY_STOP_WRITES`] copies and deletes ALL failed, the destination or
/// the backend is broken, and going on only repeats the same error for
/// every object of the scope.
const EARLY_STOP_WRITES: i64 = 20;

/// PURE: must the run stop because every write so far failed?
fn every_write_failing(attempts: i64, failures: i64) -> bool {
    attempts >= EARLY_STOP_WRITES && failures == attempts
}

/// The failure a run records when it stops because every write failed.
fn every_write_failed_message(attempts: i64, last: Option<&LifecycleFailure>) -> String {
    let last = last.map_or_else(String::new, |f| {
        if f.key.is_empty() {
            format!(" The last error: {}", f.error)
        } else {
            format!(" The last error ({}): {}", f.key, f.error)
        }
    });
    format!(
        "The run stopped early: all of its first {attempts} writes failed, so the \
         destination or the backend cannot accept writes. Fix that, then run the rule \
         again.{last}"
    )
}

/// How long a preview answers again without a new walk of the rule's
/// scope.
const PREVIEW_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// One preview: the walk (shared by every caller that asks while it runs)
/// and, once it ended, when.
type PreviewCell =
    Arc<tokio::sync::OnceCell<(std::time::Instant, Result<LifecycleRunOutcome, String>)>>;

/// What a preview answers for: rule name, rule definition, candidate cap
/// and engine.
type PreviewKey = (String, String, usize, usize);

/// The previews of the last [`PREVIEW_TTL`], each with the live counters
/// of its walk.
static PREVIEWS: std::sync::Mutex<Vec<(PreviewKey, PreviewCell, Arc<PreviewProgress>)>> =
    std::sync::Mutex::new(Vec::new());

/// The live counters of a preview walk, updated after each page, so the
/// GUI can show the walk advance while the preview request waits.
#[derive(Debug, Default)]
struct PreviewProgress {
    /// A walk runs now. A walk whose request went away stops (its future
    /// is dropped) and a later request starts it again, so this is not
    /// the same as "the cell has no result yet".
    running: std::sync::atomic::AtomicBool,
    /// When the running walk started (Unix milliseconds).
    started_ms: std::sync::atomic::AtomicI64,
    scanned: std::sync::atomic::AtomicI64,
    affected: std::sync::atomic::AtomicI64,
}

/// Marks a walk as running until it ends or its future is dropped.
struct WalkRunning<'a>(&'a PreviewProgress);

impl Drop for WalkRunning<'_> {
    fn drop(&mut self) {
        self.0
            .running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

impl PreviewProgress {
    /// Start a walk: counters from zero, running until the guard drops.
    fn start(&self) -> WalkRunning<'_> {
        use std::sync::atomic::Ordering::Relaxed;
        self.scanned.store(0, Relaxed);
        self.affected.store(0, Relaxed);
        self.started_ms
            .store(Utc::now().timestamp_millis(), Relaxed);
        self.running.store(true, Relaxed);
        WalkRunning(self)
    }

    /// Publish the counters of the walk so far.
    fn publish(&self, out: &LifecycleRunOutcome) {
        use std::sync::atomic::Ordering::Relaxed;
        self.scanned.store(out.objects_scanned, Relaxed);
        self.affected.store(out.objects_affected, Relaxed);
    }
}

/// `GET /jobs/lifecycle:<rule>/preview`: the preview walk of a rule that
/// runs now, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    pub objects_scanned: i64,
    /// Matches so far. A retain-newest rule knows its matches only at the
    /// end of the walk, so this stays 0 for it while the walk runs.
    pub objects_affected: i64,
}

/// The progress of the running preview walk of `rule_name`. When several
/// walks of the rule run (an edit of its definition while one walks), the
/// newest one answers.
pub fn preview_status(rule_name: &str) -> PreviewStatus {
    use std::sync::atomic::Ordering::Relaxed;
    let previews = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    previews
        .iter()
        .filter(|((name, ..), _, progress)| name == rule_name && progress.running.load(Relaxed))
        .map(|(_, _, progress)| progress)
        .max_by_key(|progress| progress.started_ms.load(Relaxed))
        .map_or(
            PreviewStatus {
                running: false,
                started_at: None,
                objects_scanned: 0,
                objects_affected: 0,
            },
            |progress| PreviewStatus {
                running: true,
                started_at: chrono::DateTime::from_timestamp_millis(
                    progress.started_ms.load(Relaxed),
                )
                .map(|at| at.to_rfc3339()),
                objects_scanned: progress.scanned.load(Relaxed),
                objects_affected: progress.affected.load(Relaxed),
            },
        )
}

/// The read-only estimate of a run of `rule`. The Jobs drawer asks for it
/// when it opens the Preview tab and again in the Run-now confirmation,
/// and each request walked the whole scope (a HEAD per delta). A preview
/// of the same definition on the same engine now answers from the last
/// walk for a minute (`PREVIEW_TTL`), and callers that ask while a walk runs wait
/// for that walk instead of starting another. A run of the rule drops its
/// previews. The run itself reuses the preview's metadata (the scan
/// cache), so Run-now right after a preview sends no HEAD for an object
/// that did not change.
///
/// `fresh` (the GUI's "Refresh preview"): never answer from a walk that
/// already ended; a walk that is still running is joined, not doubled.
pub async fn preview_rule(
    jobs_db: Option<Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_candidates: usize,
    fresh: bool,
) -> Result<LifecycleRunOutcome, String> {
    let key = (
        rule.name.clone(),
        serde_json::to_string(rule).unwrap_or_default(),
        max_candidates,
        Arc::as_ptr(engine) as usize,
    );
    let (cell, progress) = {
        let mut previews = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
        previews.retain(|(k, cell, _)| {
            cell.get().is_none_or(|(at, result)| {
                result.is_ok() && at.elapsed() < PREVIEW_TTL && !(fresh && *k == key)
            })
        });
        match previews.iter().find(|(k, ..)| *k == key) {
            Some((_, cell, progress)) => (cell.clone(), progress.clone()),
            None => {
                let cell = PreviewCell::default();
                let progress = Arc::new(PreviewProgress::default());
                previews.push((key, cell.clone(), progress.clone()));
                (cell, progress)
            }
        }
    };
    let (_, result) = cell
        .get_or_init(|| async {
            let _running = progress.start();
            // Preview never writes → no maintenance gate needed.
            let result = run_or_preview(
                None,
                jobs_db,
                engine,
                rule,
                max_candidates,
                false,
                None,
                None,
                Some(&progress),
            )
            .await
            .map(|mut out| {
                out.computed_at = Some(chrono::Utc::now().to_rfc3339());
                out
            });
            (std::time::Instant::now(), result)
        })
        .await;
    result.clone()
}

/// Drop the previews of `rule_name`: a run changed what they show.
fn forget_previews(rule_name: &str) {
    let mut previews = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    previews.retain(|((name, ..), ..)| name != rule_name);
}

#[allow(clippy::too_many_arguments)]
pub async fn run_rule(
    db: Option<Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_failures_retained: u32,
    triggered_by: &str,
    next_due_delay_secs: i64,
    lease: Option<RunLease>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> Result<LifecycleRunOutcome, String> {
    let run_id = begin_run(db.as_ref(), rule, triggered_by).await?;
    run_begun_rule(
        db,
        engine,
        rule,
        max_failures_retained,
        run_id,
        next_due_delay_secs,
        lease,
        maintenance_gate,
    )
    .await
}

/// Open the run-history row (`running`) and return its id. Split from the run
/// so an async caller (admin run-now) can answer with the id before the run.
pub async fn begin_run(
    db: Option<&Arc<Mutex<JobsDb>>>,
    rule: &LifecycleRule,
    triggered_by: &str,
) -> Result<Option<i64>, String> {
    let Some(db) = db else { return Ok(None) };
    let started_at = super::current_unix_seconds();
    let db = db.lock().await;
    db.lifecycle_ensure_state(&rule.name, started_at)
        .map_err(|err| err.to_string())?;
    db.lifecycle_begin_run(&rule.name, started_at, triggered_by)
        .map(Some)
        .map_err(|err| err.to_string())
}

/// Execute a run whose history row `begin_run` opened, and settle that row.
#[allow(clippy::too_many_arguments)]
pub async fn run_begun_rule(
    db: Option<Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_failures_retained: u32,
    run_id: Option<i64>,
    next_due_delay_secs: i64,
    lease: Option<RunLease>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> Result<LifecycleRunOutcome, String> {
    let keeper = spawn_lease_keeper(db.clone(), &rule.name, lease.clone());
    let lease_alive = keeper
        .as_ref()
        .map_or_else(LeaseAlive::always, LeaseKeeper::alive);
    let heartbeat = RunLeaseGuard {
        keeper,
        release: db
            .clone()
            .zip(lease.as_ref())
            .map(|(db, l)| (db, rule.name.clone(), l.owner.clone())),
    };

    // Visible to GET /jobs while it runs, and stoppable by pause / kill.
    let live = super::live::register(&rule.name, run_id);
    let ctx = RunContext {
        run_id,
        max_failures_retained,
        lease,
        lease_alive,
        live: Some(live.run().clone()),
    };
    let outcome_result = run_or_preview(
        db.clone(),
        db.clone(),
        engine,
        rule,
        max_failures_retained as usize,
        true,
        Some(ctx.clone()),
        maintenance_gate.clone(),
        None,
    )
    .await;
    // Normal exit: stop renewing. The caller releases the lease it took.
    heartbeat.finish();
    forget_previews(&rule.name);

    let mut outcome = match outcome_result {
        Ok(outcome) => outcome,
        Err(err) => {
            if let (Some(db), Some(run_id)) = (db.as_ref(), run_id) {
                {
                    let db = db.lock().await;
                    db.lifecycle_record_failure(
                        &rule.name,
                        LifecycleFailureInsert {
                            run_id: Some(run_id),
                            occurred_at: super::current_unix_seconds(),
                            bucket: &rule.bucket,
                            object_key: "",
                            error_message: &err,
                        },
                        max_failures_retained,
                    )
                    .map_err(|db_err| db_err.to_string())?;
                    let finished_at = super::current_unix_seconds();
                    db.lifecycle_finish_run(
                        run_id,
                        &rule.name,
                        "failed",
                        finished_at,
                        LifecycleRunTotals {
                            errors: 1,
                            ..LifecycleRunTotals::default()
                        },
                        finished_at.saturating_add(next_due_delay_secs.max(1)),
                    )
                    .map_err(|db_err| db_err.to_string())?;
                }
            }
            return Err(err);
        }
    };
    outcome.run_id = run_id;

    if let (Some(db), Some(run_id)) = (db.as_ref(), run_id) {
        let totals = outcome.totals();
        let finished_at = super::current_unix_seconds();
        let db = db.lock().await;
        db.lifecycle_finish_run(
            run_id,
            &rule.name,
            &outcome.status,
            finished_at,
            totals,
            finished_at.saturating_add(next_due_delay(&outcome.status, next_due_delay_secs)),
        )
        .map_err(|err| err.to_string())?;
    }

    Ok(outcome)
}

/// The run's lease heartbeat, stopped on EVERY exit. A panic in the run (a
/// spawned run-now) or a dropped run future skips the caller's release, and
/// a detached heartbeat renewed that lease forever: run-now, the scheduler
/// and rule delete were refused until a restart. So the drop also releases.
struct RunLeaseGuard {
    keeper: Option<LeaseKeeper>,
    /// (db, rule, owner) to release on an abnormal exit.
    release: Option<(Arc<Mutex<JobsDb>>, String, String)>,
}

impl RunLeaseGuard {
    /// Normal exit: the caller releases the lease itself.
    fn finish(mut self) {
        self.release = None;
    }
}

impl Drop for RunLeaseGuard {
    fn drop(&mut self) {
        // Stop renewing first (dropping the keeper aborts it).
        self.keeper.take();
        let Some((db, rule, owner)) = self.release.take() else {
            return;
        };
        // No runtime (shutdown): the lease lapses after its TTL.
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = db.lock().await.lifecycle_release_lease(&rule, &owner);
            });
        }
    }
}

#[derive(Clone)]
struct RunContext {
    run_id: Option<i64>,
    max_failures_retained: u32,
    lease: Option<RunLease>,
    lease_alive: LeaseAlive,
    /// Where the run publishes its counters, and reads a pause or kill.
    live: Option<Arc<super::live::LiveRun>>,
}

impl RunContext {
    fn stop_requested(&self) -> Option<super::live::StopRequest> {
        self.live.as_ref().and_then(|l| l.stop_requested())
    }

    fn publish(&self, out: &LifecycleRunOutcome) {
        if let Some(live) = &self.live {
            live.publish(out.counters());
        }
    }
}

/// The operator's stop request for the run of `ctx`, if any.
fn stop_of(ctx: Option<&RunContext>) -> Option<super::live::StopRequest> {
    ctx.and_then(RunContext::stop_requested)
}

/// `db`: the run's state (none for a preview); `scan_db`: where the scan
/// cache persists (the jobs DB, also for a preview); `progress`: where a
/// preview publishes its counters after each page.
#[allow(clippy::too_many_arguments)]
async fn run_or_preview(
    db: Option<Arc<Mutex<JobsDb>>>,
    scan_db: Option<Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    response_cap: usize,
    execute: bool,
    ctx: Option<RunContext>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
    progress: Option<&PreviewProgress>,
) -> Result<LifecycleRunOutcome, String> {
    // retain-newest is SET-RELATIVE (it ranks the whole prefix) and takes a
    // dedicated collect→rank→act path — it must never touch the per-object age
    // machinery below (which would parse a nonexistent expire_after and apply a
    // per-object decision that can't express "keep newest N").
    if let LifecycleAction::RetainNewest(action) = &rule.action {
        return run_or_preview_retain_newest(
            db,
            scan_db,
            engine,
            rule,
            action,
            response_cap,
            execute,
            ctx,
            maintenance_gate,
            progress,
        )
        .await;
    }

    let expire_after_str = rule.expire_after.as_deref().ok_or_else(|| {
        format!(
            "lifecycle rule '{}' {} action requires expire_after",
            rule.name,
            rule.action.kind()
        )
    })?;
    let expire_after = humantime::parse_duration(expire_after_str)
        .map_err(|err| format!("expire_after={expire_after_str} invalid: {err}"))?;
    let expire_after = ChronoDuration::from_std(expire_after)
        .map_err(|err| format!("expire_after={expire_after_str} out of range: {err}"))?;
    let expire_before = Utc::now() - expire_after;
    let (include_globs, exclude_globs) = compile_rule_globs(rule).map_err(|err| err.to_string())?;
    let prefix = lifecycle_prefix(rule);
    let page_size = rule.batch_size.clamp(1, 10_000);
    // EXECUTE runs resume from the persisted cursor (a crash/restart no
    // longer re-lists a huge bucket from page 0). Previews always start
    // fresh — they are read-only estimates of a full pass.
    // The cursor is scope-stamped: a token issued against one
    // bucket/prefix is meaningless (or worse, silently skips keys) on
    // another, so a redefined same-named rule starts fresh.
    let cursor_scope = format!("{}|{}", rule.bucket, prefix);
    let mut resume_token: Option<String> = None;
    if execute {
        if let Some(db) = db.as_ref() {
            let db = db.lock().await;
            let state = db
                .lifecycle_load_state(&rule.name)
                .map_err(|err| err.to_string())?;
            match state {
                Some(st) if st.cursor_scope.as_deref() == Some(cursor_scope.as_str()) => {
                    resume_token = st.continuation_token;
                }
                Some(st) if st.continuation_token.is_some() => {
                    tracing::info!(
                        "Lifecycle rule '{}' was redefined ({} -> {}) — dropping the \
                         stale resume cursor",
                        rule.name,
                        st.cursor_scope.as_deref().unwrap_or("<none>"),
                        cursor_scope
                    );
                    let _ = db.lifecycle_set_continuation_token(&rule.name, None, &cursor_scope);
                }
                _ => {}
            }
        }
    }
    let mut pager = Pager::resuming(resume_token);
    let mut next_due = None;
    // A maintenance job stopped the run: it did not finish its scan.
    let mut deferred = false;
    let mut out = LifecycleRunOutcome {
        run_id: ctx.as_ref().and_then(|c| c.run_id),
        rule_name: rule.name.clone(),
        status: if execute { "succeeded" } else { "preview" }.to_string(),
        ..LifecycleRunOutcome::default()
    };
    let scan = super::scan_cache::ScanCache::of(engine, scan_db);
    let lease_alive = ctx
        .as_ref()
        .map_or_else(LeaseAlive::always, |c| c.lease_alive.clone());
    // Asked before each key of a delete batch: a lost lease, a bucket
    // that a maintenance job gates, or an operator pause / kill stops the
    // batch between keys.
    let proceed = || {
        lease_alive.is_alive()
            && !maintenance_write_bucket_busy(rule, maintenance_gate.as_ref())
            && stop_of(ctx.as_ref()).is_none()
    };
    // Set when an operator pause or kill stopped the run.
    let mut stopped_by = None;

    'pages: while let Some(page_idx) = pager.begin_page() {
        if let Some(stop) = stop_of(ctx.as_ref()) {
            stopped_by = Some(stop);
            break 'pages;
        }
        // Acquire the gate write-window BEFORE the busy check (acquire-then-
        // recheck) so a concurrently-arming maintenance drain either waits for
        // this page's writes or we defer below with the guards dropped (H22).
        let _write_windows = if execute {
            begin_write_windows(rule, maintenance_gate.as_ref())
        } else {
            Vec::new()
        };
        // Mid-run maintenance defer: if a write bucket (source or transition
        // dest) becomes gated mid-run, stop — cursor preserved, resumes when the
        // maintenance job clears. Mirrors the replication RunControl fix (#12);
        // preview never writes, so it's execute-only.
        if execute && maintenance_write_bucket_busy(rule, maintenance_gate.as_ref()) {
            info!(
                "lifecycle rule '{}' deferring mid-run — a write bucket is under maintenance",
                rule.name
            );
            deferred = true;
            break 'pages;
        }
        if execute
            && !renew_run_lease(&db, rule, ctx.as_ref(), &mut out.failures, response_cap).await?
        {
            out.errors += 1;
            break 'pages;
        }

        let page = list_lifecycle_page(&scan, engine, rule, &prefix, page_size, pager.token())
            .await
            .map_err(|(failure, err)| {
                (
                    failure,
                    format!("list lifecycle page {page_idx} failed: {err}"),
                )
            });
        let page = match page {
            Ok(page) => page,
            Err((failure, err)) if execute => {
                // Poison-token guard: only a REFUSED token on the first page
                // of a resumed run is the cursor's fault; clear it so the
                // next run starts fresh. A transient error was retried with
                // the same token, and keeps the cursor.
                if pager.restarts_on(failure) {
                    if let Some(db) = db.as_ref() {
                        let db = db.lock().await;
                        let _ =
                            db.lifecycle_set_continuation_token(&rule.name, None, &cursor_scope);
                    }
                }
                out.errors += 1;
                push_failure(&mut out.failures, response_cap, String::new(), err.clone());
                record_failure(&db, rule, ctx.as_ref(), "", &err).await?;
                break 'pages;
            }
            Err((_, err)) => return Err(err),
        };

        // This page's deletes, run as ONE engine batch after the plan.
        let mut deletes: Vec<PlannedDelete> = Vec::new();
        for (key, meta) in page.objects {
            // A pause or kill stops at the next object: the page is not
            // done, so the cursor stays before it.
            if execute {
                if let Some(stop) = stop_of(ctx.as_ref()) {
                    stopped_by = Some(stop);
                    break;
                }
                if let Some(ctx) = ctx.as_ref() {
                    ctx.publish(&out);
                }
                let stall = crate::config::test_seams::test_seams().lifecycle_object_stall_ms;
                if stall > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(stall)).await;
                }
            }
            out.objects_scanned += 1;
            let decision = plan_object(
                rule,
                &key,
                &meta,
                expire_before,
                &include_globs,
                &exclude_globs,
            );
            if let Ok(decision) = &decision {
                next_due = earlier_due(next_due, decision, meta.created_at, expire_after);
            }
            match decision {
                Err(err) => {
                    out.errors += 1;
                    let msg = err.to_string();
                    push_failure(&mut out.failures, response_cap, key.clone(), msg.clone());
                    if execute {
                        record_failure(&db, rule, ctx.as_ref(), &key, &msg).await?;
                    }
                }
                Ok(Decision::Skip { reason }) => {
                    out.objects_skipped += 1;
                    if matches!(reason, SkipReason::NotExpired) {
                        out.objects_not_due += 1;
                    } else {
                        out.objects_excluded += 1;
                    }
                    if !matches!(reason, SkipReason::NotExpired) {
                        debug!(
                            "lifecycle rule '{}' skipped key {:?}: {:?}",
                            rule.name, key, reason
                        );
                    }
                }
                Ok(Decision::Apply { action }) => {
                    if out.candidates.len() < response_cap {
                        let (action_name, destination_bucket, destination_key, delete_source) =
                            preview_action_fields(&action);
                        out.candidates.push(PreviewObject {
                            bucket: rule.bucket.clone(),
                            key: key.clone(),
                            action: action_name.to_string(),
                            destination_bucket,
                            destination_key,
                            delete_source_after_success: delete_source,
                            created_at: meta.created_at.to_rfc3339(),
                            size: meta.file_size,
                            stored_size: meta.stored_size(),
                        });
                    }
                    if execute {
                        match plan_action(db.as_ref(), engine, rule, &key, &meta, &action).await {
                            Ok(Planned::Delete(delete)) => deletes.push(delete),
                            Ok(Planned::Copied { bytes, delete }) => {
                                out.write_attempts += 1;
                                out.objects_affected += 1;
                                out.bytes_affected += bytes as i64;
                                out.stored_bytes_affected += meta.stored_size() as i64;
                                // Copy mode: done. Move mode: a move once
                                // its source delete (in the batch) succeeds.
                                if delete.is_none() {
                                    out.objects_copied += 1;
                                }
                                deletes.extend(delete);
                            }
                            Ok(Planned::Skipped) => {
                                out.objects_skipped += 1;
                            }
                            Err(err) => {
                                out.write_attempts += 1;
                                out.write_failures += 1;
                                out.errors += 1;
                                let msg = err.to_string();
                                push_failure(
                                    &mut out.failures,
                                    response_cap,
                                    key.clone(),
                                    msg.clone(),
                                );
                                record_failure(&db, rule, ctx.as_ref(), &key, &msg).await?;
                            }
                        }
                    } else {
                        out.objects_affected += 1;
                        out.bytes_affected += meta.file_size as i64;
                        out.stored_bytes_affected += meta.stored_size() as i64;
                        match &action {
                            PlannedLifecycleAction::Delete => out.objects_deleted += 1,
                            PlannedLifecycleAction::Transition {
                                delete_source_after_success: true,
                                ..
                            } => out.objects_moved += 1,
                            PlannedLifecycleAction::Transition { .. } => out.objects_copied += 1,
                        }
                    }
                }
            }
        }

        if let Some(progress) = progress {
            progress.publish(&out);
        }
        if let Some(ctx) = ctx.as_ref() {
            ctx.publish(&out);
        }
        if stopped_by.is_some() {
            // Copies already made stay; the page's deletes do not run.
            break 'pages;
        }

        if !deletes.is_empty() {
            let stopped = delete_planned(
                db.as_ref(),
                engine,
                rule,
                deletes,
                &proceed,
                ctx.as_ref(),
                &mut out,
                response_cap,
            )
            .await?;
            if let Some(ctx) = ctx.as_ref() {
                ctx.publish(&out);
            }
            if stopped {
                // The page is not done: the cursor stays where it was, so
                // the next run plans this page again.
                stopped_by = stop_of(ctx.as_ref());
                break 'pages;
            }
        }
        if execute && every_write_failing(out.write_attempts, out.write_failures) {
            let msg = every_write_failed_message(out.write_attempts, out.failures.last());
            warn!("lifecycle rule '{}': {msg}", rule.name);
            out.errors += 1;
            push_failure(&mut out.failures, response_cap, String::new(), msg.clone());
            record_failure(&db, rule, ctx.as_ref(), "", &msg).await?;
            break 'pages;
        }

        let more = pager.advance(page.is_truncated, page.next_continuation_token);
        if execute {
            if let Some(db) = db.as_ref() {
                let db = db.lock().await;
                // Persist the resumable cursor after every page; on a
                // complete pass the pager normalizes it to None, which
                // clears the cursor so the next run starts from the top.
                db.lifecycle_set_continuation_token(&rule.name, pager.token(), &cursor_scope)
                    .map_err(|err| err.to_string())?;
            }
        }
        if !more {
            break 'pages;
        }
    }

    out.next_due_at = next_due.map(|at| at.to_rfc3339());
    if out.errors > 0 {
        out.status = "failed".to_string();
    } else if deferred {
        out.status = DEFERRED_RUN_STATUS.to_string();
    }
    if let Some(stop) = stopped_by {
        settle_stopped(&db, rule, &cursor_scope, stop, &mut out).await;
    }
    if let Some(ctx) = ctx.as_ref() {
        ctx.publish(&out);
    }
    Ok(out)
}

/// The status of a run that a maintenance job stopped (a write bucket
/// became gated): nobody paused the rule, and the run did not finish.
/// Same word as replication's (`DEFERRED_STOP_STATUS`).
pub(crate) const DEFERRED_RUN_STATUS: &str = crate::replication::worker::DEFERRED_STOP_STATUS;

/// Why retain-newest stopped its collect when a maintenance job started.
const MAINTENANCE_DEFER_REASON: &str = "a write bucket is under maintenance";

/// A deferred run is due again a minute later: the scheduler checks the
/// gate before it runs the rule, so a closed gate defers it again at no
/// cost. Any other run waits the rule's delay.
fn next_due_delay(status: &str, delay_secs: i64) -> i64 {
    if status == DEFERRED_RUN_STATUS {
        DEFERRED_RETRY_SECS
    } else {
        delay_secs.max(1)
    }
}

const DEFERRED_RETRY_SECS: i64 = 60;

/// A run that an operator pause or kill stopped: status `paused` (the
/// cursor stays, a resume continues there) or `cancelled` (the cursor is
/// dropped, the next run starts from the top).
async fn settle_stopped(
    db: &Option<Arc<Mutex<JobsDb>>>,
    rule: &LifecycleRule,
    cursor_scope: &str,
    stop: super::live::StopRequest,
    out: &mut LifecycleRunOutcome,
) {
    info!(
        "lifecycle rule '{}' stopped by the operator ({}) after {} object(s)",
        rule.name,
        stop.run_status(),
        out.objects_scanned
    );
    out.status = stop.run_status().to_string();
    if stop == super::live::StopRequest::Kill {
        if let Some(db) = db.as_ref() {
            let _ =
                db.lock()
                    .await
                    .lifecycle_set_continuation_token(&rule.name, None, cursor_scope);
        }
    }
}

/// Upper bound on objects collected for a single retain-newest pass. The decision
/// is set-relative, so we must hold the whole candidate set in memory. Backup
/// prefixes are tiny; this cap only fires on a pathological prefix, and when it
/// does we FAIL the run rather than rank a truncated set (which could delete an
/// object that is actually in the newest N). ~200k candidates ≈ a few MB.
const MAX_RETAIN_NEWEST_CANDIDATES: usize = 200_000;

/// Dedicated collect→rank→act path for `retain-newest` rules.
///
/// Unlike the age path this is NOT resumable mid-prefix: the keep/delete decision
/// needs the COMPLETE candidate set, so a half-collected set is meaningless. The
/// collect phase is read-only, so restarting it after a crash is free and correct;
/// only the act phase mutates, and `engine.delete` is idempotent.
#[allow(clippy::too_many_arguments)]
async fn run_or_preview_retain_newest(
    db: Option<Arc<Mutex<JobsDb>>>,
    scan_db: Option<Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    action: &LifecycleRetainNewestAction,
    response_cap: usize,
    execute: bool,
    ctx: Option<RunContext>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
    progress: Option<&PreviewProgress>,
) -> Result<LifecycleRunOutcome, String> {
    // Defensive guard: a count-0 retain rule would delete every qualifying
    // object in the prefix. The Deserialize impl and validate_lifecycle already
    // reject it, but a delete path gets belt-and-suspenders — refuse to run,
    // delete nothing, regardless of how the rule was constructed.
    if action.count == 0 {
        return Err(format!(
            "lifecycle rule '{}' retain-newest count is 0 — refusing to run (would delete the \
             whole prefix)",
            rule.name
        ));
    }

    let (include_globs, exclude_globs) = compile_rule_globs(rule).map_err(|err| err.to_string())?;
    let prefix = lifecycle_prefix(rule);
    let page_size = rule.batch_size.clamp(1, 10_000);

    let qualify = QualifySpec {
        min_size_bytes: action.qualify.min_size_bytes,
        min_age: parse_chrono_duration_opt(action.qualify.min_age.as_deref(), "qualify.min_age")?,
    };
    let protect_younger_than = parse_chrono_duration_opt(
        action.protect_younger_than.as_deref(),
        "protect_younger_than",
    )?;

    let mut out = LifecycleRunOutcome {
        run_id: ctx.as_ref().and_then(|c| c.run_id),
        rule_name: rule.name.clone(),
        status: if execute { "succeeded" } else { "preview" }.to_string(),
        ..LifecycleRunOutcome::default()
    };
    let scan = super::scan_cache::ScanCache::of(engine, scan_db);

    // ── Collect phase (read-only): the full candidate set, structurally filtered ──
    // Keep the FileMetadata alongside each candidate so the act phase can emit the
    // delete event without a second metadata fetch.
    let mut metas: std::collections::HashMap<String, crate::types::FileMetadata> =
        std::collections::HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut pager = Pager::fresh();
    // retain-newest is SET-RELATIVE and has NO resume cursor: ranking over a
    // partial collect would delete objects that are globally in the newest N.
    // ANY early break out of the collect loop (maintenance defer, lease loss)
    // must therefore ABORT before rank/act — never delete over a partial set.
    let mut incomplete_collect: Option<&'static str> = None;

    'pages: while let Some(page_idx) = pager.begin_page() {
        if execute && stop_of(ctx.as_ref()).is_some() {
            incomplete_collect = Some("stopped by the operator");
            break 'pages;
        }
        // Mid-run maintenance defer: a write bucket (source or transition dest)
        // became gated mid-run. Abort — the next scheduled run re-collects the
        // WHOLE set once maintenance clears. execute-only (preview never writes).
        if execute && maintenance_write_bucket_busy(rule, maintenance_gate.as_ref()) {
            info!(
                "lifecycle rule '{}' deferring mid-run — a write bucket is under maintenance",
                rule.name
            );
            incomplete_collect = Some(MAINTENANCE_DEFER_REASON);
            break 'pages;
        }
        if execute
            && !renew_run_lease(&db, rule, ctx.as_ref(), &mut out.failures, response_cap).await?
        {
            out.errors += 1;
            incomplete_collect = Some("replication lease lost");
            break 'pages;
        }
        let page = list_lifecycle_page(&scan, engine, rule, &prefix, page_size, pager.token())
            .await
            .map_err(|(_, err)| format!("list lifecycle page {page_idx} failed: {err}"))?;

        out.objects_scanned += page.objects.len() as i64;
        for (key, meta) in page.objects {
            // Same structural guards as the age path (the pure plan_object's
            // first three checks). Globs/internal/marker filtering happens HERE,
            // BEFORE the size/age qualify ranking in plan_retain_newest.
            if key.ends_with('/')
                || is_internal_key(&key)
                || exclude_globs.is_match(&key)
                || (!include_globs.is_empty() && !include_globs.is_match(&key))
            {
                out.objects_skipped += 1;
                out.objects_excluded += 1;
                continue;
            }

            if candidates.len() >= MAX_RETAIN_NEWEST_CANDIDATES {
                // Truncating the set could delete an object that is actually in
                // the newest N — never silently keep the wrong set.
                let msg = format!(
                    "retain-newest candidate cap ({MAX_RETAIN_NEWEST_CANDIDATES}) exceeded for \
                     prefix {:?}; refusing to rank a truncated set — narrow the prefix or add \
                     include_globs",
                    prefix
                );
                warn!("lifecycle rule '{}': {}", rule.name, msg);
                out.errors += 1;
                push_failure(&mut out.failures, response_cap, String::new(), msg.clone());
                record_failure(&db, rule, ctx.as_ref(), "", &msg).await?;
                out.status = "failed".to_string();
                return Ok(out);
            }
            candidates.push(Candidate {
                key: key.clone(),
                created_at: meta.created_at,
                size: meta.file_size,
            });
            metas.insert(key, meta);
        }
        if let Some(progress) = progress {
            progress.publish(&out);
        }
        if let Some(ctx) = ctx.as_ref() {
            ctx.publish(&out);
        }

        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break 'pages;
        }
    }

    // An early break (maintenance defer / lease loss) left the candidate set
    // PARTIAL — abort before rank/act. retain-newest can't resume a partial set
    // (set-relative, no cursor), so it just doesn't run this tick; the next full
    // collect handles it. A maintenance defer is NOT an error (status stays
    // succeeded, no work done); a lease loss already stamped errors above.
    if !retain_newest_may_delete(
        incomplete_collect.is_some(),
        pager.truncated_by_page_budget(),
    ) {
        // Partial collect — abort before rank/act (never delete over a partial
        // set). An early break (defer/lease) is handled here; a budget overrun
        // falls into the loud failure below.
        if let Some(reason) = incomplete_collect {
            info!(
                "lifecycle rule '{}' retain-newest deferred before ranking ({}) — no objects \
                 deleted this run (a full re-collect will run next tick)",
                rule.name, reason
            );
            if out.errors > 0 {
                out.status = "failed".to_string();
            } else if reason == MAINTENANCE_DEFER_REASON {
                out.status = DEFERRED_RUN_STATUS.to_string();
            }
            if let Some(stop) = stop_of(ctx.as_ref()) {
                out.status = stop.run_status().to_string();
            }
            return Ok(out);
        }
    }

    // Page-budget truncation is the SAME data-loss hazard as the candidate cap:
    // ranking "keep newest N" over a listing that stopped at MAX_JOB_PAGES could
    // delete an object that is actually in the newest N. Refuse rather than
    // delete over a partial set (retain-newest is set-relative — see #4).
    if pager.truncated_by_page_budget() {
        let msg = format!(
            "retain-newest listing hit the page budget ({} pages) for prefix {:?}; refusing to \
             rank a truncated set — narrow the prefix or add include_globs",
            crate::job_loop::MAX_JOB_PAGES,
            prefix
        );
        warn!("lifecycle rule '{}': {}", rule.name, msg);
        out.errors += 1;
        push_failure(&mut out.failures, response_cap, String::new(), msg.clone());
        record_failure(&db, rule, ctx.as_ref(), "", &msg).await?;
        out.status = "failed".to_string();
        return Ok(out);
    }

    // ── Rank phase (pure): the entire data-loss-sensitive decision ──
    let plan = plan_retain_newest(
        &candidates,
        action.count,
        &qualify,
        protect_younger_than,
        now(),
    );

    out.objects_ignored = plan.ignored.len() as i64;
    out.objects_protected = plan.protected.len() as i64;

    // Preview rows = the would-delete set (what the operator most needs to see).
    for c in &plan.delete {
        if out.candidates.len() >= response_cap {
            break;
        }
        out.candidates.push(PreviewObject {
            bucket: rule.bucket.clone(),
            key: c.key.clone(),
            action: "delete".to_string(),
            destination_bucket: None,
            destination_key: None,
            delete_source_after_success: false,
            created_at: c.created_at.to_rfc3339(),
            size: c.size,
            stored_size: stored_size_of(&metas, &c.key, c.size),
        });
    }

    // ── Act phase ──
    if !execute {
        out.objects_affected = plan.delete.len() as i64;
        out.objects_deleted = out.objects_affected;
        out.bytes_affected = plan.delete.iter().map(|c| c.size as i64).sum();
        out.stored_bytes_affected = plan
            .delete
            .iter()
            .map(|c| stored_size_of(&metas, &c.key, c.size) as i64)
            .sum();
        return Ok(out);
    }

    // Register the delete window with the gate for the whole act loop so a
    // reencrypt/migrate drain waits for it instead of racing our deletes (H22).
    let _write_windows = begin_write_windows(rule, maintenance_gate.as_ref());
    // Re-check after acquiring: if a job armed between collect and here, defer
    // the entire act phase rather than delete into a bucket being rewritten.
    if maintenance_write_bucket_busy(rule, maintenance_gate.as_ref()) {
        info!(
            "lifecycle rule '{}' retain-newest deferring act phase — a write bucket is under \
             maintenance; no objects deleted this run",
            rule.name
        );
        return Ok(out);
    }

    // TOCTOU guard: the ranking used a snapshot taken during collect. An
    // overwrite since then is a new object that might rank in the KEEP set,
    // so each delete is conditional on the snapshot. One batch: one
    // reclaim check per folder, several folders at once.
    let deletes = plan
        .delete
        .iter()
        .map(|c| PlannedDelete {
            key: c.key.clone(),
            snapshot: match metas.get(&c.key) {
                Some(meta) => Snapshot::of(meta),
                None => Snapshot {
                    created_at: c.created_at,
                    etag: String::new(),
                    size: c.size,
                },
            },
            size: c.size,
            stored_size: stored_size_of(&metas, &c.key, c.size),
            action: "retain-newest",
            counted: true,
        })
        .collect();
    let lease_alive = ctx
        .as_ref()
        .map_or_else(LeaseAlive::always, |c| c.lease_alive.clone());
    let proceed = || {
        lease_alive.is_alive()
            && !maintenance_write_bucket_busy(rule, maintenance_gate.as_ref())
            && stop_of(ctx.as_ref()).is_none()
    };
    delete_planned(
        db.as_ref(),
        engine,
        rule,
        deletes,
        &proceed,
        ctx.as_ref(),
        &mut out,
        response_cap,
    )
    .await?;

    if out.errors > 0 {
        out.status = "failed".to_string();
    }
    if let Some(stop) = stop_of(ctx.as_ref()) {
        out.status = stop.run_status().to_string();
    }
    if let Some(ctx) = ctx.as_ref() {
        ctx.publish(&out);
    }
    Ok(out)
}

/// The bytes on the backend of a collected candidate (its logical size
/// when the listing did not keep its metadata).
fn stored_size_of(
    metas: &std::collections::HashMap<String, crate::types::FileMetadata>,
    key: &str,
    size: u64,
) -> u64 {
    metas.get(key).map_or(size, |m| m.stored_size())
}

/// Parse an optional humantime string into a chrono Duration, mapping errors to
/// the run-failure string with the field name for context.
fn parse_chrono_duration_opt(
    value: Option<&str>,
    field: &str,
) -> Result<Option<ChronoDuration>, String> {
    match value {
        None => Ok(None),
        Some(s) => {
            let std = humantime::parse_duration(s)
                .map_err(|err| format!("{field}={s} invalid: {err}"))?;
            let chrono = ChronoDuration::from_std(std)
                .map_err(|err| format!("{field}={s} out of range: {err}"))?;
            Ok(Some(chrono))
        }
    }
}

/// `Utc::now()` indirection so the retain path uses a single timestamp for the
/// whole pass (consistent qualify/protect cutoffs across the candidate set).
fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

fn preview_action_fields(
    action: &PlannedLifecycleAction,
) -> (&'static str, Option<String>, Option<String>, bool) {
    match action {
        PlannedLifecycleAction::Delete => ("delete", None, None, false),
        PlannedLifecycleAction::Transition {
            destination_bucket,
            destination_key,
            delete_source_after_success,
        } => (
            "transition",
            Some(destination_bucket.clone()),
            Some(destination_key.clone()),
            *delete_source_after_success,
        ),
    }
}

/// Result of re-checking an object right before a delete.
#[derive(Debug, PartialEq, Eq)]
enum DeleteCheck {
    /// Same generation as the snapshot: delete.
    Proceed,
    /// Overwritten since the snapshot: a new object the rule never judged.
    Changed,
    /// Already deleted by another writer.
    Gone,
    /// HEAD failed: fail closed (never delete what we cannot see).
    HeadFailed(String),
}

/// What the listing saw of an object: enough to tell a fresh HEAD of the
/// SAME upload from an overwrite.
#[derive(Debug, Clone)]
struct Snapshot {
    created_at: chrono::DateTime<Utc>,
    etag: String,
    size: u64,
}

impl Snapshot {
    fn of(meta: &crate::types::FileMetadata) -> Self {
        Self {
            created_at: meta.created_at,
            etag: meta.etag(),
            size: meta.file_size,
        }
    }
}

/// How far apart two clocks may stamp one upload: the proxy's
/// `dg-created-at` (before the upload) and the backend's LastModified (at
/// its end). The SigV4 clock-skew default.
const STAMP_SKEW_SECS: i64 = 900;

/// Pure: compare the snapshot with a fresh HEAD. `created_at` is the
/// generation marker: every overwrite stamps a new one. But the two sides
/// can read it from different clocks: on S3 with a cold metadata cache the
/// lite LIST entry has the BACKEND's LastModified (end of the upload), and
/// the HEAD has `dg-created-at`, which the PROXY stamped before the upload.
/// Those never matched, and lifecycle never deleted such an object. So one
/// upload is also the same ETag and size with stamps within
/// [`STAMP_SKEW_SECS`]. An overwrite with the SAME bytes inside that window
/// counts as the same object (it holds exactly what the rule judged).
fn classify_delete_check(
    snapshot: &Snapshot,
    head: Result<&crate::types::FileMetadata, &crate::deltaglider::EngineError>,
) -> DeleteCheck {
    match head {
        Ok(current) if same_object(snapshot, current) => DeleteCheck::Proceed,
        Ok(_) => DeleteCheck::Changed,
        Err(crate::deltaglider::EngineError::NotFound(_)) => DeleteCheck::Gone,
        Err(e) => DeleteCheck::HeadFailed(format!("re-check before delete failed: {e}")),
    }
}

fn same_object(snapshot: &Snapshot, current: &crate::types::FileMetadata) -> bool {
    same_generation(snapshot.created_at, current.created_at)
        || (current.etag() == snapshot.etag
            && current.file_size == snapshot.size
            && (current.created_at - snapshot.created_at)
                .num_seconds()
                .abs()
                <= STAMP_SKEW_SECS)
}

/// Pure: do two `created_at` values name one generation? Backends report it
/// at different precisions: on S3 the lite LIST entry has milliseconds,
/// HEAD's `dg-created-at` microseconds, and a Last-Modified header whole
/// seconds. An exact compare called every such object overwritten, and
/// lifecycle never deleted it. Both values are truncated to the COARSER
/// precision of the two; equal precision still compares exactly, so an
/// overwrite within one second on a precise backend counts as a change.
fn same_generation(a: chrono::DateTime<Utc>, b: chrono::DateTime<Utc>) -> bool {
    use chrono::Timelike;
    // Nanoseconds per unit of a value's precision (1 ns .. 1 s).
    let unit = |t: chrono::DateTime<Utc>| {
        let n = t.nanosecond() % 1_000_000_000;
        [1_000_000_000u32, 1_000_000, 1_000]
            .into_iter()
            .find(|u| n.is_multiple_of(*u))
            .unwrap_or(1)
    };
    let u = unit(a).max(unit(b));
    let trunc = |t: chrono::DateTime<Utc>| (t.timestamp(), (t.nanosecond() % 1_000_000_000) / u);
    trunc(a) == trunc(b)
}

/// One planned delete of a lifecycle run. Every lifecycle delete is
/// conditional: the plan was made on a listing snapshot, and a delete by
/// key would remove a newer overwrite. The engine checks the object under
/// the object's write lock that every write holds, so no overwrite lands between
/// the check and the delete (a HEAD, then a delete by key, lost one).
#[derive(Debug)]
struct PlannedDelete {
    key: String,
    /// What the listing saw: the delete goes only while the object is
    /// still this one.
    snapshot: Snapshot,
    /// The size the run reports for it (logical).
    size: u64,
    /// Its bytes on the backend.
    stored_size: u64,
    /// The `action` of its delete event.
    action: &'static str,
    /// Counted as an affected object. A transition's source delete is not:
    /// its copy was.
    counted: bool,
}

impl PlannedDelete {
    fn of(
        key: &str,
        meta: &crate::types::FileMetadata,
        action: &'static str,
        counted: bool,
    ) -> Self {
        Self {
            key: key.to_string(),
            snapshot: Snapshot::of(meta),
            size: meta.file_size,
            stored_size: meta.stored_size(),
            action,
            counted,
        }
    }
}

/// What a run does with one planned object before its page's deletes.
#[derive(Debug)]
enum Planned {
    /// Delete it in the page's batch.
    Delete(PlannedDelete),
    /// Transitioned (copied this many bytes); `delete` is its source
    /// delete, for the page's batch.
    Copied {
        bytes: u64,
        delete: Option<PlannedDelete>,
    },
    /// Nothing to do: the destination of a copy-mode transition already
    /// holds the object.
    Skipped,
}

async fn plan_action(
    db: Option<&Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    action: &PlannedLifecycleAction,
) -> Result<Planned, Box<dyn std::error::Error + Send + Sync>> {
    match action {
        PlannedLifecycleAction::Delete => Ok(Planned::Delete(PlannedDelete::of(
            key, meta, "delete", true,
        ))),
        PlannedLifecycleAction::Transition {
            destination_bucket,
            destination_key,
            delete_source_after_success,
        } => {
            // A copy-mode transition meets the same expired objects on every
            // run: skip one the destination already holds.
            if !*delete_source_after_success {
                if let Ok(dest) = engine.head(destination_bucket, destination_key).await {
                    if crate::transfer::content_verdict(meta, Some(&dest))
                        == crate::transfer::ContentVerdict::Same
                    {
                        return Ok(Planned::Skipped);
                    }
                }
            }
            let copied = copy_object_with_retries(
                engine,
                ObjectTransferRequest {
                    source_bucket: &rule.bucket,
                    source_key: key,
                    destination_bucket,
                    destination_key,
                    provenance: Some(TransferProvenance {
                        metadata_key: LIFECYCLE_RULE_METADATA_KEY,
                        metadata_value: &rule.name,
                    }),
                    strip_user_metadata_keys: &[],
                    operation: "lifecycle transition",
                    upload_concurrency: None,
                    keep_created_at: true,
                },
            )
            .await
            .map_err(|err| copy_failure_message(destination_bucket, destination_key, &err))?;
            append_lifecycle_transition_event(
                db,
                rule,
                key,
                meta,
                destination_bucket,
                destination_key,
                copied.content_length(),
                *delete_source_after_success,
            )
            .await;
            // Overwritten since the listing: the copy is done, but the new
            // source is not ours to delete (the batch's condition).
            // The LOGICAL size: a server-side copy moves no bytes through
            // the proxy (`bytes_copied` is 0), and the run reports what
            // it archived, not what it streamed.
            Ok(Planned::Copied {
                bytes: copied.content_length(),
                delete: delete_source_after_success
                    .then(|| PlannedDelete::of(key, meta, "transition-source-delete", false)),
            })
        }
    }
}

/// The failure of a transition copy names where it went: the source key
/// alone does not say which destination refused it.
fn copy_failure_message(
    destination_bucket: &str,
    destination_key: &str,
    err: &dyn std::fmt::Display,
) -> String {
    format!("copy to {destination_bucket}/{destination_key} failed: {err}")
}

/// Run `deletes` as ONE engine batch (`DeltaGliderEngine::delete_batch`):
/// each one conditional on its snapshot, one reclaim check per folder,
/// several folders at once. A per-key delete paid 7 HEAD requests and a
/// LIST of its folder per key, one key at a time. Each deleted key's event
/// is buffered as soon as the key ends, and the page's events are stored in
/// one outbox insert after the batch. `proceed` stops the batch between
/// keys; the result is `true` when it did (the caller stops the run).
#[allow(clippy::too_many_arguments)]
async fn delete_planned(
    db: Option<&Arc<Mutex<JobsDb>>>,
    engine: &DynEngine,
    rule: &LifecycleRule,
    deletes: Vec<PlannedDelete>,
    proceed: &(dyn Fn() -> bool + Send + Sync),
    ctx: Option<&RunContext>,
    out: &mut LifecycleRunOutcome,
    response_cap: usize,
) -> Result<bool, String> {
    let events = Arc::new(DeleteEvents {
        db: db.cloned(),
        rule_name: rule.name.clone(),
        bucket: rule.bucket.clone(),
        expire_after: rule.expire_after.clone(),
        actions: deletes.iter().map(|d| (d.key.clone(), d.action)).collect(),
        pending: Default::default(),
    });
    let publisher = events.clone();
    let on_outcome = move |key: &str, outcome: &DeleteOutcome| publisher.publish(key, outcome);
    let items = deletes
        .iter()
        .map(|d| {
            let snapshot = &d.snapshot;
            DeleteItem::only_if(
                d.key.as_str(),
                move |current: &crate::types::FileMetadata| {
                    classify_delete_check(snapshot, Ok(current)) == DeleteCheck::Proceed
                },
            )
        })
        .collect();
    let outcomes = engine
        .delete_batch(
            &rule.bucket,
            items,
            DeleteHooks {
                proceed: Some(proceed),
                on_outcome: Some(&on_outcome),
            },
        )
        .await;
    events.flush().await;
    let mut stopped = false;
    for (d, outcome) in deletes.iter().zip(outcomes) {
        match outcome {
            DeleteOutcome::Deleted(_) if d.counted => {
                out.write_attempts += 1;
                out.objects_affected += 1;
                out.objects_deleted += 1;
                out.bytes_affected += d.size as i64;
                out.stored_bytes_affected += d.stored_size as i64;
            }
            DeleteOutcome::Deleted(_) => {
                // A move's source delete: the copy was counted already.
                out.write_attempts += 1;
                out.objects_moved += 1;
            }
            DeleteOutcome::Skipped => {
                // Not tried: the run stops (lease, gate, pause or kill),
                // and the next run plans this key again.
                stopped = true;
                if !d.counted {
                    out.objects_copied += 1;
                }
            }
            DeleteOutcome::Changed | DeleteOutcome::NotFound => {
                if d.counted {
                    out.objects_skipped += 1;
                } else {
                    // The copy stands; the source changed, so it stays.
                    out.objects_copied += 1;
                }
                debug!(
                    "lifecycle rule '{}' did not delete {:?}: {outcome:?}",
                    rule.name, d.key
                );
            }
            DeleteOutcome::Failed(err) => {
                out.write_attempts += 1;
                out.write_failures += 1;
                if !d.counted {
                    out.objects_copied += 1;
                }
                out.errors += 1;
                let msg = if d.counted {
                    format!("delete failed: {err}")
                } else {
                    format!("the copy succeeded, but the source delete failed: {err}")
                };
                push_failure(&mut out.failures, response_cap, d.key.clone(), msg.clone());
                record_failure(&db.cloned(), rule, ctx, &d.key, &msg).await?;
            }
        }
    }
    if stopped && !ctx.is_none_or(|c| c.lease_alive.is_alive()) {
        let msg = "lost lifecycle lease; stopping run before more work";
        out.errors += 1;
        push_failure(
            &mut out.failures,
            response_cap,
            String::new(),
            msg.to_string(),
        );
        record_failure(&db.cloned(), rule, ctx, "", msg).await?;
    } else if stopped && stop_of(ctx).is_none() {
        info!(
            "lifecycle rule '{}' deferring mid-run — a write bucket is under maintenance",
            rule.name
        );
    }
    Ok(stopped)
}

/// The delete events of one batch: its `on_outcome` hook buffers each
/// deleted key's event, and [`DeleteEvents::flush`] stores the page's
/// events in one outbox insert (one commit, not one per key).
struct DeleteEvents {
    db: Option<Arc<Mutex<JobsDb>>>,
    rule_name: String,
    bucket: String,
    expire_after: Option<String>,
    /// key → the event's `action`.
    actions: std::collections::HashMap<String, &'static str>,
    /// Events of keys already deleted, not stored yet.
    pending: parking_lot::Mutex<Vec<NewEvent>>,
}

impl Drop for DeleteEvents {
    /// A run dropped in the middle of a batch (an abort) never reached
    /// `flush`: the keys it deleted still get their events, through the
    /// batched writer.
    fn drop(&mut self) {
        let pending = std::mem::take(&mut *self.pending.lock());
        if let (Some(db), false) = (self.db.clone(), pending.is_empty()) {
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move { crate::event_outbox::append_events(&db, pending).await });
            }
        }
    }
}

impl DeleteEvents {
    fn publish(
        &self,
        key: &str,
        outcome: &DeleteOutcome,
    ) -> futures::future::BoxFuture<'static, ()> {
        let (Some(db), DeleteOutcome::Deleted(meta)) = (&self.db, outcome) else {
            return Box::pin(async {});
        };
        if !crate::event_outbox::outbox_has_readers(db) {
            return Box::pin(async {});
        }
        let event = NewEvent::new(
            EventKind::LifecycleExpired,
            self.bucket.as_str(),
            key,
            EventSource::Lifecycle,
            super::current_unix_seconds(),
            serde_json::json!({
                "rule_name": &self.rule_name,
                "action": self.actions.get(key).copied().unwrap_or("delete"),
                "expire_after": &self.expire_after,
                "created_at": meta.created_at.to_rfc3339(),
                "content_length": meta.file_size,
            }),
        );
        self.pending.lock().push(event);
        Box::pin(async {})
    }

    /// Store the buffered events in one insert.
    async fn flush(&self) {
        let events = std::mem::take(&mut *self.pending.lock());
        let (Some(db), false) = (&self.db, events.is_empty()) else {
            return;
        };
        if let Err(err) = db.lock().await.event_outbox_insert_many(&events) {
            warn!(
                "lifecycle rule '{}' could not append {} delete event(s): {err}",
                self.rule_name,
                events.len()
            );
        }
    }
}

/// One page of the rule's scope, each object with its logical metadata
/// (from the scan cache while its stored object is unchanged). A
/// transient error is retried with the same token; `Err` carries what the
/// failure means for the cursor.
async fn list_lifecycle_page(
    scan: &super::scan_cache::Scan,
    engine: &DynEngine,
    rule: &LifecycleRule,
    prefix: &str,
    page_size: u32,
    token: Option<&str>,
) -> Result<ListObjectsPage, (ListFailure, crate::deltaglider::EngineError)> {
    let mut attempt = 0u32;
    loop {
        let err = match scan
            .list_page(engine, &rule.bucket, prefix, page_size, token)
            .await
        {
            Ok(page) => return Ok(page),
            Err(err) => err,
        };
        attempt += 1;
        let failure = ListFailure::of(&err);
        let Some(pause) = list_retry_delay(failure, attempt) else {
            return Err((failure, err));
        };
        warn!(
            "lifecycle rule '{}' listing failed ({err}); listing the same page again",
            rule.name
        );
        tokio::time::sleep(pause).await;
    }
}

/// One planned action, its delete run as a batch of one (tests).
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum ActionOutcome {
    /// Deleted and/or copied this many bytes.
    Acted(u64),
    /// The object changed or vanished since the listing: nothing done.
    Skipped,
}

#[cfg(test)]
async fn execute_action(
    db: Option<&Arc<Mutex<JobsDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    action: &PlannedLifecycleAction,
) -> Result<ActionOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let mut out = LifecycleRunOutcome::default();
    let delete = match plan_action(db, engine, rule, key, meta, action).await? {
        Planned::Skipped => return Ok(ActionOutcome::Skipped),
        Planned::Delete(d) => Some(d),
        Planned::Copied { bytes, delete } => {
            out.objects_affected = 1;
            out.bytes_affected = bytes as i64;
            delete
        }
    };
    if let Some(d) = delete {
        delete_planned(db, engine, rule, vec![d], &|| true, None, &mut out, 10).await?;
    }
    if let Some(f) = out.failures.first() {
        return Err(f.error.clone().into());
    }
    Ok(match out.objects_affected {
        0 => ActionOutcome::Skipped,
        _ => ActionOutcome::Acted(out.bytes_affected as u64),
    })
}

/// The run's lease keeper (`None` without a DB or a lease: nothing to renew).
fn spawn_lease_keeper(
    db: Option<Arc<Mutex<JobsDb>>>,
    rule_name: &str,
    lease: Option<RunLease>,
) -> Option<LeaseKeeper> {
    let db = db?;
    let lease = lease?;
    let rule_name = rule_name.to_string();
    Some(LeaseKeeper::spawn(
        format!("lifecycle rule '{rule_name}'"),
        std::time::Duration::from_secs(lease.heartbeat_secs.max(1) as u64),
        std::time::Duration::from_secs(lease.ttl_secs.max(1) as u64),
        move || {
            let (db, rule_name, lease) = (db.clone(), rule_name.clone(), lease.clone());
            async move {
                let db = db.lock().await;
                LeaseError::from_renewal(db.lifecycle_renew_lease(
                    &rule_name,
                    &lease.owner,
                    super::current_unix_seconds(),
                    lease.ttl_secs,
                ))
            }
        },
    ))
}

/// True when any bucket this rule WRITES to (source-for-deletes + transition
/// destination) is under a maintenance write-gate. Used to defer a lifecycle
/// run mid-sweep so it never writes into a bucket being migrated/re-encrypted.
/// PURE: may a retain-newest run proceed to rank + DELETE? Only when the
/// candidate collect saw the WHOLE prefix — a partial collect (early break
/// from a maintenance defer / lease loss, or a page-budget overrun) would rank
/// "keep newest N" over an incomplete set and delete objects that are globally
/// in the newest N. retain-newest is set-relative with no resume cursor, so a
/// partial set is NEVER safe to act on.
fn retain_newest_may_delete(collect_incomplete: bool, budget_truncated: bool) -> bool {
    !collect_incomplete && !budget_truncated
}

fn maintenance_write_bucket_busy(
    rule: &LifecycleRule,
    gate: Option<&Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> bool {
    let Some(gate) = gate else {
        return false;
    };
    super::planner::rule_write_buckets(rule)
        .into_iter()
        .any(|b| gate.is_busy(b))
}

/// Register this rule's write buckets with the maintenance gate for the
/// duration of a write window, so a reencrypt/migrate job that arms concurrently
/// has its drain_inflight_writes WAIT for our in-flight writes instead of racing
/// engine.store/delete on the same key (H22). Acquire BEFORE the is_busy check
/// so the acquire-then-recheck closes the TOCTOU: either our +1 is visible to
/// the drain, or the busy check sees the job and we defer with guards dropped.
fn begin_write_windows(
    rule: &LifecycleRule,
    gate: Option<&Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> Vec<crate::maintenance::gate::WriteGuard> {
    match gate {
        None => Vec::new(),
        Some(gate) => super::planner::rule_write_buckets(rule)
            .into_iter()
            .map(|b| gate.begin_write(b))
            .collect(),
    }
}

async fn renew_run_lease(
    db: &Option<Arc<Mutex<JobsDb>>>,
    rule: &LifecycleRule,
    ctx: Option<&RunContext>,
    failures: &mut Vec<LifecycleFailure>,
    response_cap: usize,
) -> Result<bool, String> {
    let Some(ctx) = ctx else {
        return Ok(true);
    };
    let Some(lease) = ctx.lease.as_ref() else {
        return Ok(true);
    };
    let Some(db) = db else {
        return Ok(true);
    };
    // A lease the keeper gave up is not renewed here: a refused renewal
    // lapsed it, and a lapsed lease never renews anyway.
    let renewed = {
        let guard = db.lock().await;
        if !ctx.lease_alive.is_alive() {
            false
        } else {
            match guard.lifecycle_renew_lease(
                &rule.name,
                &lease.owner,
                super::current_unix_seconds(),
                lease.ttl_secs,
            ) {
                Ok(renewed) => renewed,
                // Not a lost lease: the heartbeat retries it and marks the
                // lease lost only when the TTL runs out.
                Err(err) => {
                    warn!(
                        "Lifecycle lease renew for rule '{}' failed ({err}); going on",
                        rule.name
                    );
                    true
                }
            }
        }
    };
    if renewed {
        return Ok(true);
    }

    let msg = "lost lifecycle lease; stopping run before more work";
    push_failure(failures, response_cap, String::new(), msg.to_string());
    record_failure(&Some(db.clone()), rule, Some(ctx), "", msg).await?;
    Ok(false)
}

async fn record_failure(
    db: &Option<Arc<Mutex<JobsDb>>>,
    rule: &LifecycleRule,
    ctx: Option<&RunContext>,
    key: &str,
    error_message: &str,
) -> Result<(), String> {
    let Some(db) = db.as_ref() else {
        return Ok(());
    };
    let Some(ctx) = ctx else {
        return Ok(());
    };
    let Some(run_id) = ctx.run_id else {
        return Ok(());
    };
    let db = db.lock().await;
    db.lifecycle_record_failure(
        &rule.name,
        LifecycleFailureInsert {
            run_id: Some(run_id),
            occurred_at: super::current_unix_seconds(),
            bucket: &rule.bucket,
            object_key: key,
            error_message,
        },
        ctx.max_failures_retained,
    )
    .map_err(|err| err.to_string())
}

#[allow(clippy::too_many_arguments)]
async fn append_lifecycle_transition_event(
    db: Option<&Arc<Mutex<JobsDb>>>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    destination_bucket: &str,
    destination_key: &str,
    content_length: u64,
    delete_source_after_success: bool,
) {
    let Some(db) = db.filter(|db| crate::event_outbox::outbox_has_readers(db)) else {
        return;
    };
    let event = NewEvent::new(
        EventKind::LifecycleTransitioned,
        destination_bucket,
        destination_key,
        EventSource::Lifecycle,
        super::current_unix_seconds(),
        serde_json::json!({
            "rule_name": &rule.name,
            "action": "transition",
            "source_bucket": &rule.bucket,
            "source_key": key,
            "destination_bucket": destination_bucket,
            "destination_key": destination_key,
            "expire_after": &rule.expire_after,
            "created_at": meta.created_at.to_rfc3339(),
            "content_length": content_length,
            "delete_source_after_success": delete_source_after_success,
        }),
    );
    let db = db.lock().await;
    if let Err(err) = db.event_outbox_insert(&event) {
        warn!(
            "lifecycle rule '{}' could not append transition event for {:?}: {}",
            rule.name, key, err
        );
    }
}

fn push_failure(failures: &mut Vec<LifecycleFailure>, cap: usize, key: String, error: String) {
    if failures.len() < cap {
        failures.push(LifecycleFailure { key, error });
    }
}

#[cfg(test)]
mod tests {
    use super::retain_newest_may_delete;
    use super::{execute_action, PlannedLifecycleAction};
    use crate::config::Config;
    use crate::config_sections::LifecycleRule;
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::DynStorageBackend;
    use crate::storage::FilesystemBackend;
    use std::sync::Arc;

    async fn fs_engine(dir: &std::path::Path) -> Arc<DynEngine> {
        let backend: Box<DynStorageBackend<'static>> =
            DynStorageBackend::new_box(FilesystemBackend::new(dir.to_path_buf()).await.unwrap());
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.create_bucket("b").await.ok();
        engine.create_bucket("dst").await.ok();
        Arc::new(engine)
    }

    fn rule() -> LifecycleRule {
        LifecycleRule {
            name: "r".to_string(),
            enabled: true,
            bucket: "b".to_string(),
            prefix: String::new(),
            action: Default::default(),
            expire_after: Some("1d".to_string()),
            include_globs: vec![],
            exclude_globs: vec![],
            batch_size: 100,
        }
    }

    /// Store `key`, snapshot its metadata (what the listing saw), then overwrite
    /// it: the snapshot is now stale.
    async fn stale_snapshot(engine: &DynEngine, key: &str) -> crate::types::FileMetadata {
        engine
            .store("b", key, b"old generation", None, Default::default())
            .await
            .unwrap();
        let snapshot = engine.head("b", key).await.unwrap();
        // created_at must differ between the two generations.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        engine
            .store("b", key, b"new generation!", None, Default::default())
            .await
            .unwrap();
        snapshot
    }

    /// D6: an age delete acts on a listing snapshot. An overwrite after the
    /// listing is a NEW object that the rule never judged; it must survive.
    #[tokio::test]
    async fn age_delete_spares_an_object_overwritten_after_listing() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        let snapshot = stale_snapshot(&engine, "k.bin").await;
        let _ = execute_action(
            None,
            &engine,
            &rule(),
            "k.bin",
            &snapshot,
            &PlannedLifecycleAction::Delete,
        )
        .await
        .unwrap();
        assert!(
            engine.head("b", "k.bin").await.is_ok(),
            "the newer overwrite was deleted"
        );
    }

    /// D6: the transition source delete must also spare a newer overwrite.
    #[tokio::test]
    async fn transition_source_delete_spares_an_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        let snapshot = stale_snapshot(&engine, "t.bin").await;
        let _ = execute_action(
            None,
            &engine,
            &rule(),
            "t.bin",
            &snapshot,
            &PlannedLifecycleAction::Transition {
                destination_bucket: "dst".to_string(),
                destination_key: "t.bin".to_string(),
                delete_source_after_success: true,
            },
        )
        .await
        .unwrap();
        assert!(
            engine.head("b", "t.bin").await.is_ok(),
            "the newer overwrite was deleted after the transition copy"
        );
    }

    #[test]
    fn delete_check_truth_table() {
        use super::{classify_delete_check, DeleteCheck};
        use crate::deltaglider::EngineError;
        let mut meta = crate::types::FileMetadata::fallback(
            "k".into(),
            1,
            "e".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        let t = super::Snapshot::of(&meta);
        assert_eq!(classify_delete_check(&t, Ok(&meta)), DeleteCheck::Proceed);
        // An overwrite: new bytes, a new stamp.
        meta.md5 = "f".into();
        meta.created_at = t.created_at + chrono::Duration::milliseconds(1);
        assert_eq!(classify_delete_check(&t, Ok(&meta)), DeleteCheck::Changed);
        meta.created_at = t.created_at + chrono::Duration::seconds(1);
        assert_eq!(classify_delete_check(&t, Ok(&meta)), DeleteCheck::Changed);
        assert_eq!(
            classify_delete_check(&t, Err(&EngineError::NotFound("k".into()))),
            DeleteCheck::Gone
        );
        assert!(matches!(
            classify_delete_check(&t, Err(&EngineError::InvalidArgument("boom".into()))),
            DeleteCheck::HeadFailed(_)
        ));
    }

    /// S3 LIST reports LastModified in milliseconds; HEAD's Last-Modified
    /// header has whole seconds. A passthrough object without DG metadata
    /// therefore always looked overwritten, and lifecycle never deleted it.
    #[test]
    fn delete_check_compares_at_the_coarser_precision() {
        use super::{classify_delete_check, DeleteCheck};
        use chrono::TimeZone;
        // Another ETag: only the stamp decides here.
        let snap = |t| super::Snapshot {
            created_at: t,
            etag: "\"other\"".into(),
            size: 1,
        };
        let listed = chrono::Utc.timestamp_millis_opt(1_700_000_000_123).unwrap();
        let headed = crate::types::FileMetadata::fallback(
            "k".into(),
            1,
            "e".into(),
            chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert_eq!(
            classify_delete_check(&snap(listed), Ok(&headed)),
            DeleteCheck::Proceed
        );
        // LIST milliseconds vs HEAD `dg-created-at` microseconds.
        let micros = crate::types::FileMetadata::fallback(
            "k".into(),
            1,
            "e".into(),
            chrono::Utc.timestamp_micros(1_700_000_000_123_456).unwrap(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        assert_eq!(
            classify_delete_check(&snap(listed), Ok(&micros)),
            DeleteCheck::Proceed
        );
        let later = chrono::Utc.timestamp_millis_opt(1_700_000_000_124).unwrap();
        assert_eq!(
            classify_delete_check(&snap(later), Ok(&micros)),
            DeleteCheck::Changed
        );
    }

    /// CI on MinIO: an object PUT through the proxy, cold metadata cache.
    /// The lite LIST entry carries the BACKEND's LastModified (end of the
    /// upload, 20:45:26.889); the re-HEAD returns `dg-created-at`, which the
    /// PROXY stamped before the upload (20:45:26.8412345, or even the
    /// previous second). Two clocks, two instants: no precision rule makes
    /// them equal, and lifecycle never deleted the object.
    #[test]
    fn delete_check_accepts_the_backend_and_proxy_stamps_of_one_upload() {
        use super::{classify_delete_check, DeleteCheck, Snapshot};
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let head = |created: &str, md5: &str| {
            crate::types::FileMetadata::fallback(
                "app.log".into(),
                1,
                md5.into(),
                at(created),
                None,
                crate::types::StorageInfo::Passthrough,
            )
        };
        let listed = Snapshot {
            created_at: at("2026-09-25T20:45:26.889+00:00"),
            etag: head(
                "2026-09-25T20:45:26.889+00:00",
                "9dd4e461268c8034f5c8564e155c67a6",
            )
            .etag(),
            size: 1,
        };
        for stamp in [
            "2026-09-25T20:45:26.841234500Z",
            "2026-09-25T20:45:25.999999Z",
        ] {
            assert_eq!(
                classify_delete_check(
                    &listed,
                    Ok(&head(stamp, "9dd4e461268c8034f5c8564e155c67a6"))
                ),
                DeleteCheck::Proceed,
                "{stamp}"
            );
        }
        // An overwrite: new bytes, or the same bytes stamped well after.
        assert_eq!(
            classify_delete_check(&listed, Ok(&head("2026-09-25T20:45:26.900Z", "ffff"))),
            DeleteCheck::Changed
        );
        assert_eq!(
            classify_delete_check(
                &listed,
                Ok(&head(
                    "2026-09-25T21:45:26Z",
                    "9dd4e461268c8034f5c8564e155c67a6"
                ))
            ),
            DeleteCheck::Changed
        );
    }

    /// A copy-mode transition (source kept) acts on the same expired objects
    /// on every run. When the destination already holds the same content,
    /// the run must not copy it again.
    #[tokio::test]
    async fn copy_mode_transition_does_not_recopy_an_up_to_date_destination() {
        use super::ActionOutcome;
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        engine
            .store("b", "c.bin", b"payload", None, Default::default())
            .await
            .unwrap();
        let meta = engine.head("b", "c.bin").await.unwrap();
        let action = PlannedLifecycleAction::Transition {
            destination_bucket: "dst".to_string(),
            destination_key: "c.bin".to_string(),
            delete_source_after_success: false,
        };
        let first = execute_action(None, &engine, &rule(), "c.bin", &meta, &action)
            .await
            .unwrap();
        assert!(matches!(first, ActionOutcome::Acted(_)), "{first:?}");
        let second = execute_action(None, &engine, &rule(), "c.bin", &meta, &action)
            .await
            .unwrap();
        assert_eq!(
            second,
            ActionOutcome::Skipped,
            "the second run copied again"
        );
    }

    /// #5: a transition reports the LOGICAL size it archived. A copy inside
    /// one backend is server-side and streams no byte through the proxy
    /// (`bytes_copied` is 0), and the run, the row and the lifetime total
    /// all said 0 B.
    #[tokio::test]
    async fn a_transition_reports_the_logical_bytes() {
        use super::ActionOutcome;
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        engine
            .store("b", "m.bin", b"0123456789", None, Default::default())
            .await
            .unwrap();
        let meta = engine.head("b", "m.bin").await.unwrap();
        let action = PlannedLifecycleAction::Transition {
            destination_bucket: "dst".to_string(),
            destination_key: "m.bin".to_string(),
            delete_source_after_success: true,
        };
        let out = execute_action(None, &engine, &rule(), "m.bin", &meta, &action)
            .await
            .unwrap();
        assert_eq!(out, ActionOutcome::Acted(10));
    }

    #[test]
    fn a_run_stops_early_only_when_every_write_failed() {
        use super::{every_write_failing, EARLY_STOP_WRITES};
        assert!(!every_write_failing(0, 0));
        assert!(!every_write_failing(
            EARLY_STOP_WRITES - 1,
            EARLY_STOP_WRITES - 1
        ));
        assert!(every_write_failing(EARLY_STOP_WRITES, EARLY_STOP_WRITES));
        assert!(!every_write_failing(
            EARLY_STOP_WRITES,
            EARLY_STOP_WRITES - 1
        ));
        assert!(!every_write_failing(500, 499), "one success: go on");
    }

    /// #12: a failed transition copy names its destination.
    #[test]
    fn a_copy_failure_names_the_destination_key() {
        let msg = super::copy_failure_message("archive", "old/a.bin", &"403 AccessDenied");
        assert_eq!(msg, "copy to archive/old/a.bin failed: 403 AccessDenied");
    }

    async fn store_all(engine: &DynEngine, keys: &[&str]) {
        for key in keys {
            engine
                .store("b", key, b"payload", None, Default::default())
                .await
                .unwrap();
        }
    }

    fn jobs_db() -> Arc<tokio::sync::Mutex<crate::config_db::JobsDb>> {
        Arc::new(tokio::sync::Mutex::new(
            crate::config_db::JobsDb::in_memory("t").unwrap(),
        ))
    }

    /// #9 + #10: a run (and a preview) tells the objects kept out by a glob
    /// from the ones not old enough yet, and counts its deletes; the run
    /// history keeps the split.
    #[tokio::test]
    async fn a_run_splits_its_skips_and_counts_its_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        store_all(&engine, &["old/a.bin", "old/b.bin", "keep/c.bin"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        store_all(&engine, &["young.bin"]).await;
        let rule = LifecycleRule {
            expire_after: Some("1s".into()),
            exclude_globs: vec!["keep/**".into()],
            ..rule()
        };
        let preview = super::preview_rule(None, &engine, &rule, 10, true)
            .await
            .unwrap();
        assert_eq!(
            (
                preview.objects_deleted,
                preview.objects_excluded,
                preview.objects_not_due
            ),
            (2, 1, 1),
            "{preview:?}"
        );
        assert_eq!(preview.stored_bytes_affected, 14);
        let db = jobs_db();
        let out = super::run_rule(
            Some(db.clone()),
            &engine,
            &rule,
            10,
            "test",
            3600,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                out.objects_deleted,
                out.objects_excluded,
                out.objects_not_due,
                out.bytes_affected
            ),
            (2, 1, 1, 14),
            "{out:?}"
        );
        let runs = db.lock().await.lifecycle_recent_runs("r", 1).unwrap();
        let split = runs[0].split.expect("the split is stored");
        assert_eq!(
            (
                split.objects_deleted,
                split.objects_excluded,
                split.objects_not_due
            ),
            (2, 1, 1)
        );
    }

    /// #2: a run that an operator pause (or kill) stops ends at the next
    /// object, deletes nothing more, and says so in its status. A pause
    /// keeps the cursor; a kill drops it.
    #[tokio::test]
    async fn a_pause_or_kill_stops_the_run_at_the_next_object() {
        use crate::lifecycle::live::{self, StopRequest};
        for (stop, status) in [
            (StopRequest::Pause, "paused"),
            (StopRequest::Kill, "cancelled"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let engine = fs_engine(dir.path()).await;
            store_all(&engine, &["x/1.bin", "x/2.bin"]).await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let rule = LifecycleRule {
                name: format!("stop-{status}"),
                expire_after: Some("1ms".into()),
                ..rule()
            };
            let db = jobs_db();
            {
                let db = db.lock().await;
                db.lifecycle_ensure_state(&rule.name, 0).unwrap();
                db.lifecycle_set_continuation_token(&rule.name, Some("tok"), "b|")
                    .unwrap();
            }
            let guard = live::register(&rule.name, None);
            assert!(live::request_stop(&rule.name, stop));
            let ctx = super::RunContext {
                run_id: None,
                max_failures_retained: 10,
                lease: None,
                lease_alive: crate::background::LeaseAlive::always(),
                live: Some(guard.run().clone()),
            };
            let out = super::run_or_preview(
                Some(db.clone()),
                None,
                &engine,
                &rule,
                10,
                true,
                Some(ctx),
                None,
                None,
            )
            .await
            .unwrap();
            assert_eq!(out.status, status);
            assert_eq!(out.objects_deleted, 0);
            assert!(
                engine.head("b", "x/1.bin").await.is_ok(),
                "{status}: deleted"
            );
            let cursor = db
                .lock()
                .await
                .lifecycle_load_state(&rule.name)
                .unwrap()
                .unwrap()
                .continuation_token;
            assert_eq!(
                cursor.is_some(),
                stop == StopRequest::Pause,
                "{status}: cursor {cursor:?}"
            );
        }
    }

    /// A run that a maintenance job stops is `deferred`, not `succeeded`:
    /// it did not finish its scan. It keeps its cursor and is due again a
    /// minute later, not one rule delay later.
    #[tokio::test]
    async fn a_run_that_maintenance_stops_is_deferred_and_due_soon() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        store_all(&engine, &["x/1.bin", "x/2.bin"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let rule = LifecycleRule {
            name: "gated".into(),
            expire_after: Some("1ms".into()),
            ..rule()
        };
        let db = jobs_db();
        {
            let db = db.lock().await;
            db.lifecycle_ensure_state(&rule.name, 0).unwrap();
            db.lifecycle_set_continuation_token(&rule.name, Some("tok"), "b|")
                .unwrap();
        }
        let gate = Arc::new(crate::maintenance::gate::MaintenanceGate::new());
        gate.set_busy("b");
        let before = crate::lifecycle::current_unix_seconds();

        let out = super::run_rule(
            Some(db.clone()),
            &engine,
            &rule,
            10,
            "scheduler",
            3600,
            None,
            Some(gate),
        )
        .await
        .unwrap();

        assert_eq!(out.status, "deferred");
        assert_eq!(out.objects_deleted, 0);
        assert!(engine.head("b", "x/1.bin").await.is_ok());
        let state = db
            .lock()
            .await
            .lifecycle_load_state(&rule.name)
            .unwrap()
            .unwrap();
        assert_eq!(state.last_status, "deferred");
        assert!(state.continuation_token.is_some(), "the cursor is kept");
        assert!(
            state.next_due_at <= before + 120,
            "due again soon, not one delay later: {}",
            state.next_due_at - before
        );
    }

    #[test]
    fn only_a_deferred_run_is_due_again_soon() {
        assert_eq!(super::next_due_delay("deferred", 3600), 60);
        assert_eq!(super::next_due_delay("succeeded", 3600), 3600);
        assert_eq!(super::next_due_delay("paused", 0), 1);
    }

    /// An unchanged object is still deleted (the guard must not block the rule).
    #[tokio::test]
    async fn age_delete_removes_an_unchanged_object() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        engine
            .store("b", "u.bin", b"x", None, Default::default())
            .await
            .unwrap();
        let snapshot = engine.head("b", "u.bin").await.unwrap();
        let _ = execute_action(
            None,
            &engine,
            &rule(),
            "u.bin",
            &snapshot,
            &PlannedLifecycleAction::Delete,
        )
        .await
        .unwrap();
        assert!(engine.head("b", "u.bin").await.is_err());
    }

    /// retain-newest may rank+delete ONLY over a complete, non-truncated
    /// collect — a partial set (early defer/lease break OR budget overrun)
    /// must never be acted on (set-relative, no resume cursor → data loss).
    /// P4.3: the delete events of one page are one outbox insert (one
    /// commit), not one per deleted object.
    #[tokio::test]
    async fn a_page_of_deletes_appends_its_events_in_one_commit() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine(dir.path()).await;
        let dbdir = tempfile::tempdir().unwrap();
        let db = Arc::new(tokio::sync::Mutex::new(
            crate::config_db::JobsDb::open_or_create(&dbdir.path().join("c.db"), "pw").unwrap(),
        ));
        let mut deletes = Vec::new();
        for i in 0..20 {
            let key = format!("k{i:02}.bin");
            engine
                .store("b", &key, b"x", None, Default::default())
                .await
                .unwrap();
            let meta = engine.head("b", &key).await.unwrap();
            deletes.push(super::PlannedDelete::of(&key, &meta, "delete", true));
        }
        let before = crate::event_outbox::pages_written(&*db.lock().await);
        let mut out = super::LifecycleRunOutcome::default();
        let stopped = super::delete_planned(
            Some(&db),
            &engine,
            &rule(),
            deletes,
            &|| true,
            None,
            &mut out,
            10,
        )
        .await
        .unwrap();
        assert!(!stopped);
        assert_eq!(out.objects_affected, 20);
        let pages = crate::event_outbox::pages_written(&*db.lock().await) - before;
        let rows = db.lock().await.event_outbox_since(0, 100).unwrap();
        assert_eq!(rows.len(), 20, "every deleted key has its event");
        assert!(
            rows.iter().all(|r| r.kind == "LifecycleExpired"),
            "{rows:?}"
        );
        assert!(pages <= 10, "one commit for the page, wrote {pages} pages");
    }

    /// A run dropped after some keys were deleted and before the page's
    /// flush still stores their events (through the batched writer).
    #[tokio::test]
    async fn a_dropped_batch_still_stores_its_buffered_events() {
        let db = Arc::new(tokio::sync::Mutex::new(
            crate::config_db::JobsDb::in_memory("pw").unwrap(),
        ));
        let events = super::DeleteEvents {
            db: Some(db.clone()),
            rule_name: "r".into(),
            bucket: "b".into(),
            expire_after: None,
            actions: Default::default(),
            pending: Default::default(),
        };
        let meta = crate::types::FileMetadata::fallback(
            "k".into(),
            1,
            "e".into(),
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        events
            .publish(
                "k",
                &crate::deltaglider::DeleteOutcome::Deleted(Box::new(meta)),
            )
            .await;
        drop(events);
        tokio::task::yield_now().await; // the spawned append queues
        crate::event_outbox::flush_appends(&db).await;
        let rows = db.lock().await.event_outbox_since(0, 10).unwrap();
        assert_eq!(rows.len(), 1, "the deleted key lost its event");
    }

    #[test]
    fn retain_newest_delete_gate() {
        assert!(
            retain_newest_may_delete(false, false),
            "complete collect → may delete"
        );
        assert!(
            !retain_newest_may_delete(true, false),
            "incomplete collect → abort"
        );
        assert!(
            !retain_newest_may_delete(false, true),
            "budget-truncated → abort"
        );
        assert!(!retain_newest_may_delete(true, true), "both → abort");
    }
}

#[cfg(test)]
mod review3_tests {
    use super::*;
    use crate::config::Config;
    use crate::deltaglider::DeltaGliderEngine;
    use crate::storage::DynStorageBackend;
    use crate::storage::FilesystemBackend;

    /// The run's lease heartbeat is a detached task, aborted only on the
    /// normal return. A run that unwinds (panic in the spawned run-now task)
    /// or whose future is dropped leaves the heartbeat renewing the lease
    /// forever: run-now, the scheduler and rule delete are refused until a
    /// restart. The future drop stands in for the unwind here.
    #[tokio::test]
    async fn review3_an_aborted_run_does_not_keep_its_lease_alive() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &Config::default(),
            None,
        ));
        engine.create_bucket("b").await.unwrap();
        for i in 0..50 {
            engine
                .store("b", &format!("k{i}.bin"), b"x", None, Default::default())
                .await
                .unwrap();
        }
        let db = Arc::new(Mutex::new(JobsDb::in_memory("k").unwrap()));
        let rule = crate::config_sections::LifecycleRule {
            name: "r".to_string(),
            enabled: true,
            bucket: "b".to_string(),
            prefix: String::new(),
            action: Default::default(),
            expire_after: Some("1d".to_string()),
            include_globs: vec![],
            exclude_globs: vec![],
            batch_size: 100,
        };
        let now = crate::lifecycle::current_unix_seconds();
        {
            let d = db.lock().await;
            d.lifecycle_ensure_state("r", now).unwrap();
            assert!(d.lifecycle_try_acquire_lease("r", "X", now, 2).unwrap());
        }
        let run_id = begin_run(Some(&db), &rule, "run-now").await.unwrap();
        let fut = run_begun_rule(
            Some(db.clone()),
            &engine,
            &rule,
            10,
            run_id,
            60,
            Some(RunLease {
                owner: "X".into(),
                ttl_secs: 2,
                heartbeat_secs: 1,
            }),
            None,
        );
        let _ = tokio::time::timeout(std::time::Duration::ZERO, fut).await;
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        let now = crate::lifecycle::current_unix_seconds();
        assert!(
            db.lock()
                .await
                .lifecycle_try_acquire_lease("r", "Y", now, 2)
                .unwrap(),
            "the lease of a run that is gone is still renewed"
        );
    }
}

#[cfg(test)]
mod heartbeat_db_error_tests {
    use super::*;

    /// A DB error on one renew (a locked or busy DB) is not a lost lease:
    /// the lease is still ours until its TTL. The heartbeat read the error
    /// as "lost" and stopped the run.
    #[tokio::test]
    async fn a_db_error_on_one_renew_does_not_lose_the_lease() {
        let db = Arc::new(Mutex::new(JobsDb::in_memory("k").unwrap()));
        let now = crate::lifecycle::current_unix_seconds();
        {
            let d = db.lock().await;
            d.lifecycle_ensure_state("r", now).unwrap();
            assert!(d.lifecycle_try_acquire_lease("r", "X", now, 30).unwrap());
            // Every statement on the table fails until it is renamed back.
            d.conn
                .execute_batch("ALTER TABLE lifecycle_state RENAME TO lifecycle_state_off")
                .unwrap();
        }
        let keeper = spawn_lease_keeper(
            Some(db.clone()),
            "r",
            Some(RunLease {
                owner: "X".into(),
                ttl_secs: 30,
                heartbeat_secs: 1,
            }),
        )
        .unwrap();
        let alive = keeper.alive();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        db.lock()
            .await
            .conn
            .execute_batch("ALTER TABLE lifecycle_state_off RENAME TO lifecycle_state")
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        drop(keeper);
        assert!(
            alive.is_alive(),
            "one failed renew marked a live lease lost"
        );
    }
}
