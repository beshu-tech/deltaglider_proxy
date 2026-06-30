// SPDX-License-Identifier: GPL-3.0-only

//! Replication worker: executes one full run of a single rule against
//! a live engine + config DB.
//!
//! What `run_rule` does (H1+H2+H3+M1 fixes wave):
//!
//! 1. Loops engine `list_objects` pages until exhaustion. After each
//!    page the worker persists `replication_state.continuation_token`
//!    so a crash mid-run resumes on the next tick instead of starting
//!    over from page 1.
//! 2. Per-object: HEAD destination, consult planner, `engine.retrieve`
//!    source, `engine.store_with_multipart_etag` (when source carries
//!    one) or `engine.store` (single-PUT objects). Preserves the H1
//!    multipart-ETag identity across replication.
//! 3. After the forward-copy pass, when `replicate_deletes` is true,
//!    paginates the destination prefix and deletes any key not present
//!    on source.
//! 4. Records per-object failures into the failure ring.
//! 5. Final status: `"failed"` when ANY copy/delete errored, else
//!    `"succeeded"`. Pre-fix the status was only flipped to failed when
//!    EVERY copy failed, so dashboards reading `last_status` got a
//!    silent partial failure.
//!
//! Resumability: after a successful complete pass the
//! `continuation_token` is cleared. If the worker crashes mid-pass,
//! `reconcile_on_boot` flips the running row to `failed` but the token
//! stays — next legitimate run resumes from the saved cursor.

use super::planner::{normalize_prefix, plan_batch};
use super::state_store::{current_unix_seconds, FailureInsert, RunTotals};
use crate::background::RunLease;
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationRule;
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::job_loop::Pager;
use crate::metrics::{bump_peak, Metrics};
use crate::transfer::{
    copy_object_with_retries, CopyStrategy, ObjectTransferRequest, TransferProvenance,
    REPLICATION_RULE_METADATA_KEY,
};
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// RAII guard for one in-flight replication object. Increments
/// `objects_inflight` (+peak) on construction and decrements on drop so the
/// gauge always settles, even on an early return/abort.
struct ObjectGuard {
    metrics: Arc<Metrics>,
}

impl ObjectGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        metrics.replication_objects_inflight.inc();
        bump_peak(
            &metrics.replication_objects_inflight,
            &metrics.replication_objects_inflight_peak,
        );
        Self { metrics }
    }
}

impl Drop for ObjectGuard {
    fn drop(&mut self) {
        self.metrics.replication_objects_inflight.dec();
    }
}

/// Test seam: when `DGP_TEST_OBJECT_BARRIER=1`, async-sleep a fixed delay
/// (`DGP_TEST_OBJECT_DELAY_MS`, default 150ms) so >=`transfers` objects are
/// co-resident → the objects-inflight peak deterministically reaches the
/// configured object concurrency. Inert in prod.
async fn maybe_object_barrier() {
    if crate::config::env_bool("DGP_TEST_OBJECT_BARRIER", false) {
        let ms: u64 = crate::config::env_parse_with_default("DGP_TEST_OBJECT_DELAY_MS", 150);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// User-metadata key stamped on objects created by replication so the
/// delete pass (H2 fix) can tell its own copies apart from objects
/// written by other rules or operators sharing the same destination
/// prefix. Value is the rule name.
///
/// Why a user-metadata key (not a system-managed marker): user-metadata
/// round-trips through both backends without any DG-specific plumbing,
/// survives encryption (per-backend SSE doesn't encrypt user-metadata),
/// and is visible to operators auditing what wrote a given object.
/// Outcome of a single run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Terminal status string (goes into `replication_run_history.status`).
    pub status: String,
    pub totals: RunTotals,
}

/// Per-run concurrency knobs (Phase B+). `transfers` = concurrent objects
/// per page; `upload_concurrency` = in-flight parts per streaming object.
#[derive(Debug, Clone, Copy)]
pub struct RunConcurrency {
    pub transfers: u32,
    pub upload_concurrency: u32,
}

impl Default for RunConcurrency {
    fn default() -> Self {
        Self {
            transfers: crate::transfer_plan::TRANSFERS as u32,
            upload_concurrency: crate::transfer_plan::UPLOAD_CONCURRENCY as u32,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_rule(
    db: Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    max_failures_retained: u32,
    object_timeout: Option<std::time::Duration>,
    object_skip_after_failures: u32,
    triggered_by: &str,
    lease: Option<RunLease>,
    concurrency: RunConcurrency,
) -> Result<(i64, RunOutcome), crate::config_db::ConfigDbError> {
    let transfers = concurrency.transfers.clamp(1, 64) as usize;
    let upload_concurrency = concurrency.upload_concurrency.clamp(1, 16) as usize;
    let started_at = current_unix_seconds();

    // Look up the saved continuation token to resume from a prior tick.
    // Cleared at the end of a successful complete pass.
    let (run_id, continuation) = {
        let db = db.lock().await;
        db.replication_ensure_state(&rule.name, started_at)?;
        let state = db.replication_load_state(&rule.name)?;
        let resume_token = state.and_then(|s| s.continuation_token);
        let id = db.replication_begin_run(&rule.name, started_at, triggered_by)?;
        (id, resume_token)
    };

    info!(
        "Replication run starting: rule='{}' src={}/{} dst={}/{} resuming={}",
        rule.name,
        rule.source.bucket,
        rule.source.prefix,
        rule.destination.bucket,
        rule.destination.prefix,
        continuation.is_some(),
    );

    let mut totals = RunTotals::default();
    let mut had_any_error = false;
    let mut hit_fatal_error = false;
    // Set when the operator pauses the rule mid-run (DB `paused` flag, re-read
    // at each page boundary). A paused stop is NOT an error: it preserves the
    // cursor so resume continues, and settles the run as "stopped".
    let mut stopped_paused = false;
    // Operator KILL: run flipped to 'cancelling' mid-flight. Checked at every
    // page boundary AND raced against the in-flight page so a wedged object
    // (e.g. stuck on a dead B2 dest) aborts immediately, not after its timeout.
    let mut killed = false;
    let cap = rule.batch_size.clamp(1, 10_000);
    let source_prefix = normalize_prefix(&rule.source.prefix);
    let lease_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat_handle =
        spawn_lease_heartbeat(db.clone(), &rule.name, lease.clone(), lease_alive.clone());

    let mut pager = Pager::resuming(continuation);
    // ── Forward-copy pass: paginate source until exhausted ──
    'pages: while let Some(page_idx) = pager.begin_page() {
        // Honor an operator pause MID-RUN: re-read the DB `paused` flag at each
        // page boundary so a long sweep stops promptly instead of running to
        // completion (the bug: pause only blocked the SCHEDULER from STARTING a
        // run, never the in-flight one). Clean stop — cursor is preserved below
        // so resume continues; not an error.
        // ponytail: this covers PAUSE (a DB flag, live-visible here). DISABLE
        // (`enabled: false`) is a config edit and the worker holds a snapshot
        // `&ReplicationRule`, so a mid-run disable is NOT yet interrupted — the
        // operator's stop button is Pause. To also honor disable mid-run, thread
        // the live SharedConfig into run_rule and re-check `rule.enabled` here.
        if {
            let db = db.lock().await;
            matches!(db.replication_load_state(&rule.name), Ok(Some(st)) if st.paused)
        } {
            info!(
                "replication rule '{}' paused mid-run — stopping after page {} (cursor preserved for resume)",
                rule.name, page_idx
            );
            stopped_paused = true;
            break 'pages;
        }

        // Operator kill at a page boundary — clean stop, cursor preserved.
        let cancel_requested = {
            let db = db.lock().await;
            db.replication_run_cancel_requested(run_id).unwrap_or(false)
        };
        if cancel_requested {
            info!(
                "replication rule '{}' killed mid-run — stopping after page {}",
                rule.name, page_idx
            );
            killed = true;
            break 'pages;
        }

        if !renew_run_lease(
            &db,
            rule,
            lease.as_ref(),
            &lease_alive,
            run_id,
            max_failures_retained,
        )
        .await?
        {
            totals.errors += 1;
            hit_fatal_error = true;
            break 'pages;
        }

        let page = match engine
            .list_objects(
                &rule.source.bucket,
                &source_prefix,
                None,
                cap,
                pager.token(),
                true,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' list page {} failed: {}",
                    rule.name, page_idx, e
                );
                // Poison-token guard: a RESUMED run whose FIRST page fails
                // to list most likely holds a backend-invalidated token —
                // clear it so the next tick starts fresh instead of
                // wedging every subsequent run on the same bad cursor.
                if pager.poisoned_resume_token() {
                    let db = db.lock().await;
                    let _ = db.replication_set_continuation_token(&rule.name, None);
                }
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("list source failed: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        };

        totals.objects_scanned += page.objects.len() as i64;

        // Plan this page. The planner heads each destination key and
        // applies the conflict policy + glob filters.
        let plan = {
            let head_engine = engine.clone();
            let dest_bucket = rule.destination.bucket.clone();
            plan_batch(&page.objects, rule, move |dest_key| {
                let engine = head_engine.clone();
                let dest_bucket = dest_bucket.clone();
                let dk = dest_key.to_string();
                async move { engine.head(&dest_bucket, &dk).await.ok() }
            })
            .await
        };

        let plan = match plan {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' page {} planner error: {}",
                    rule.name, page_idx, e
                );
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("planner error: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        };

        totals.objects_skipped += plan.skipped.len() as i64;

        // Events for this page are buffered and flushed in a single
        // locked `event_outbox_insert_many` at page completion, rather
        // than locking the DB per object. The outbox is asynchronous and
        // replication doesn't need real-time delivery, so trading
        // per-object immediacy for one lock acquisition per page is a
        // pure throughput win on large runs.
        let mut page_events: Vec<NewEvent> = Vec::with_capacity(plan.to_copy.len());

        // Renew the lease ONCE before the page's concurrent copy batch.
        // The independent heartbeat task keeps it alive during the batch;
        // we re-check `lease_alive` after the batch (and per page). This
        // preserves the single-flight-lease invariant — concurrency is
        // WITHIN one run; the lease still guarantees one worker per rule.
        if !renew_run_lease(
            &db,
            rule,
            lease.as_ref(),
            &lease_alive,
            run_id,
            max_failures_retained,
        )
        .await?
        {
            flush_page_events(&db, &rule.name, &mut page_events).await;
            totals.errors += 1;
            hit_fatal_error = true;
            break 'pages;
        }

        // Copy up to `transfers` objects concurrently. Each unit does its
        // own DB writes (failure/clear — they serialize through the shared
        // Arc<Mutex<ConfigDb>>) and returns its totals delta + optional
        // event. The page boundary is the barrier: the cursor does not
        // advance until every in-flight object of this page finishes.
        let copy_page = futures::stream::iter(plan.to_copy.clone())
            .map(|(src_key, dest_key)| {
                let db = db.clone();
                let engine = engine.clone();
                let rule_name = rule.name.clone();
                let src_bucket = rule.source.bucket.clone();
                let dst_bucket = rule.destination.bucket.clone();
                async move {
                    // Guard increments objects_inflight (+peak) on entry and
                    // decrements on drop → proves the `transfers` concurrency.
                    let _obj_guard = engine.metrics().cloned().map(ObjectGuard::new);
                    copy_one_object(
                        &db,
                        &engine,
                        &rule_name,
                        &src_bucket,
                        &dst_bucket,
                        &src_key,
                        &dest_key,
                        run_id,
                        object_timeout,
                        object_skip_after_failures,
                        upload_concurrency,
                        max_failures_retained,
                    )
                    .await
                }
            })
            .buffer_unordered(transfers)
            .collect::<Vec<_>>();

        // Race the page against a kill poll. If the operator kills the run, the
        // collect future is DROPPED — every in-flight copy_one_object drops with
        // it, aborting the underlying HTTP transfers immediately (a wedged
        // object on a dead dest dies now, not after object_timeout).
        let object_results: Vec<Result<PerObjectResult, crate::config_db::ConfigDbError>> = tokio::select! {
            biased;
            killed_now = poll_run_killed(&db, run_id) => {
                if killed_now { killed = true; }
                Vec::new()
            }
            results = copy_page => results,
        };
        if killed {
            break 'pages;
        }

        // Fold the concurrent results into totals + flags + events. DB
        // failure/clear writes already happened inside each unit; any
        // ConfigDb error is surfaced here (the first one wins).
        let mut dest_fatal = false;
        for res in object_results {
            let res = res?;
            totals.objects_copied += res.objects_copied;
            totals.objects_skipped += res.objects_skipped;
            totals.bytes_copied += res.bytes_copied;
            totals.errors += res.errors;
            totals.delta_passthrough += res.delta_passthrough;
            totals.bytes_egress_saved += res.bytes_egress_saved;
            if res.had_error {
                had_any_error = true;
            }
            if res.dest_fatal {
                dest_fatal = true;
            }
            if let Some(ev) = res.event {
                page_events.push(ev);
            }
        }
        // Destination unusable (bucket missing / over quota) — abort the run
        // instead of retrying every remaining object against a dead dest.
        if dest_fatal {
            flush_page_events(&db, &rule.name, &mut page_events).await;
            warn!(
                "replication rule '{}' aborting run: destination unusable (bucket missing or over quota)",
                rule.name
            );
            hit_fatal_error = true;
            break 'pages;
        }
        {
            let db = db.lock().await;
            db.replication_update_run_progress(run_id, totals)?;
        }

        // If the lease lapsed during the batch, stop before advancing.
        if !lease_alive.load(std::sync::atomic::Ordering::Acquire) && lease.is_some() {
            flush_page_events(&db, &rule.name, &mut page_events).await;
            totals.errors += 1;
            hit_fatal_error = true;
            break 'pages;
        }

        // Persist the cursor so the next tick can resume here if we
        // crash before the run finishes naturally, and flush this page's
        // buffered copy events in a single batched insert under the same
        // lock acquisition. Event-append is non-critical: a failure is
        // logged and the run continues (the copies themselves are
        // durable).
        let more = pager.advance(page.is_truncated, page.next_continuation_token);
        {
            // Single lock acquisition fuses the cursor persist, the run
            // progress, and the page's event flush — do not split (see
            // the throughput note above).
            let db = db.lock().await;
            db.replication_set_continuation_token(&rule.name, pager.token())?;
            db.replication_update_run_progress(run_id, totals)?;
            flush_page_events_locked(&db, &rule.name, &mut page_events);
        }

        if !more {
            break 'pages;
        }
    }

    // ── Delete-replication pass (opt-in per rule) ──
    //
    // After the forward copy completes, paginate the destination prefix
    // and delete every key whose corresponding source key is missing.
    // Only fires when forward-copy didn't hit a fatal error — partial
    // listing failures could leave us thinking source is empty when
    // it's not, and a full destination wipe would be catastrophic.
    if rule.replicate_deletes && !hit_fatal_error && !stopped_paused && !killed {
        if let Err(e) = run_delete_pass(
            db.clone(),
            engine,
            rule,
            run_id,
            &mut totals,
            &mut had_any_error,
            max_failures_retained,
        )
        .await
        {
            warn!("replication rule '{}' delete pass error: {}", rule.name, e);
            had_any_error = true;
        }
    }

    // Final status, three-way:
    // - "failed": a FATAL error (couldn't list source), OR the sweep errored
    //   AND copied NOTHING — it accomplished nothing reliable.
    // - "completed_with_errors": the sweep made PARTIAL progress — it copied
    //   some objects but ≥1 errored (e.g. a transient destination 500). The run
    //   still copied everything else; flagging it "failed" cried wolf on
    //   99.99%-good runs and buried real fatal failures in the noise.
    // - "succeeded": clean pass, zero errors.
    // A pause stop settles as "stopped" — NOT failed/completed (the sweep was
    // intentionally interrupted, not broken). It outranks the error-derived
    // states because the partial-progress that's left is by operator request.
    let status = if killed {
        "cancelled".to_string()
    } else if stopped_paused {
        "stopped".to_string()
    } else if hit_fatal_error || (had_any_error && totals.objects_copied == 0) {
        "failed".to_string()
    } else if had_any_error {
        "completed_with_errors".to_string()
    } else {
        "succeeded".to_string()
    };

    let finished_at = current_unix_seconds();
    let next_due = if hit_fatal_error {
        // Tighter retry on fatal errors so the operator-facing
        // "next due" doesn't claim a long sleep when the worker
        // gave up immediately.
        finished_at + 60
    } else {
        compute_next_due(rule, finished_at)
    };

    // Clear the continuation token on a clean complete pass — next run starts
    // from the beginning. A paused stop PRESERVES the cursor so resume picks up
    // mid-prefix; a fatal error also keeps it (poison-token guard handles bad
    // tokens separately).
    let clear_cursor_on_clean = !hit_fatal_error && !stopped_paused && !killed;

    {
        let db = db.lock().await;
        if clear_cursor_on_clean {
            db.replication_set_continuation_token(&rule.name, None)?;
        }
        db.replication_finish_run(run_id, &rule.name, &status, finished_at, totals, next_due)?;
    }
    // Settle barrier: bump AFTER the terminal row is written so a test polling
    // the run-version sees the settled run. The single chokepoint all scheduled
    // runs pass through.
    super::state_store::bump_replication_run_version();

    info!(
        "Replication run finished: rule='{}' status={} scanned={} copied={} skipped={} deleted={} errors={} bytes={}",
        rule.name,
        status,
        totals.objects_scanned,
        totals.objects_copied,
        totals.objects_skipped,
        totals.objects_deleted,
        totals.errors,
        totals.bytes_copied,
    );
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    Ok((run_id, RunOutcome { status, totals }))
}

/// Outcome of one concurrent per-object copy unit. Totals deltas are
/// folded by the caller; DB failure/clear writes already happened inside.
#[derive(Default)]
struct PerObjectResult {
    objects_copied: i64,
    objects_skipped: i64,
    bytes_copied: i64,
    errors: i64,
    had_error: bool,
    // The error means the DESTINATION is unusable for the whole run (bucket
    // missing / over quota) — the caller aborts the run instead of retrying
    // every remaining object against a dead dest.
    dest_fatal: bool,
    event: Option<NewEvent>,
    // Fast-path attribution for the successful copy (zero otherwise).
    delta_passthrough: i64,
    bytes_egress_saved: i64,
}

/// Copy one object: poison-skip check → bounded copy → record/clear the
/// per-object failure. Runs concurrently with up to `transfers` siblings;
/// all DB writes serialize through the shared `Arc<Mutex<ConfigDb>>`.
#[allow(clippy::too_many_arguments)]
async fn copy_one_object(
    db: &Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule_name: &str,
    src_bucket: &str,
    dst_bucket: &str,
    src_key: &str,
    dest_key: &str,
    run_id: i64,
    object_timeout: Option<std::time::Duration>,
    object_skip_after_failures: u32,
    upload_concurrency: usize,
    max_failures_retained: u32,
) -> Result<PerObjectResult, crate::config_db::ConfigDbError> {
    let mut out = PerObjectResult::default();

    // Test-only barrier: force >=transfers objects co-resident (inert in prod).
    maybe_object_barrier().await;

    // Poison-object guard: skip an object that has failed every run for
    // `object_skip_after_failures` consecutive runs. Reset on success below.
    if object_skip_after_failures > 0 {
        let skipped = {
            let db = db.lock().await;
            db.replication_object_skipped(rule_name, src_key, object_skip_after_failures)?
        };
        if skipped {
            out.objects_skipped = 1;
            debug!(
                "replication rule '{}' skipping poison object src={:?} (>= {} consecutive failures)",
                rule_name, src_key, object_skip_after_failures
            );
            return Ok(out);
        }
    }

    let transfer = ObjectTransferRequest {
        source_bucket: src_bucket,
        source_key: src_key,
        destination_bucket: dst_bucket,
        destination_key: dest_key,
        provenance: Some(TransferProvenance {
            metadata_key: REPLICATION_RULE_METADATA_KEY,
            metadata_value: rule_name,
        }),
        strip_user_metadata_keys: &[],
        operation: "replication",
        upload_concurrency: Some(upload_concurrency),
    };
    // Bound the copy: a stalled object fails fast instead of hanging until
    // lease lapse. `Elapsed` routes into the Err arm below.
    let copy_result = match object_timeout {
        Some(timeout) => {
            match tokio::time::timeout(timeout, copy_object_with_retries(engine, transfer)).await {
                Ok(r) => r,
                Err(_elapsed) => {
                    Err(format!("object copy timed out after {}s", timeout.as_secs()).into())
                }
            }
        }
        None => copy_object_with_retries(engine, transfer).await,
    };

    match copy_result {
        Ok(outcome) => {
            let bytes_copied = outcome.bytes_copied;
            out.objects_copied = 1;
            out.bytes_copied = bytes_copied as i64;
            // Only the fast path is counted; bytes_egress_saved is computed once
            // on the outcome (non-zero only for DeltaPassthrough).
            out.bytes_egress_saved = outcome.bytes_egress_saved as i64;
            if outcome.strategy == CopyStrategy::DeltaPassthrough {
                out.delta_passthrough = 1;
            }
            {
                let db = db.lock().await;
                db.replication_clear_object_failure(rule_name, src_key)?;
            }
            out.event = Some(NewEvent::new(
                EventKind::ReplicationObjectCopied,
                dst_bucket,
                dest_key,
                EventSource::Replication,
                current_unix_seconds(),
                serde_json::json!({
                    "rule_name": rule_name,
                    "source_bucket": src_bucket,
                    "source_key": src_key,
                    "destination_bucket": dst_bucket,
                    "destination_key": dest_key,
                    "content_length": bytes_copied,
                    "strategy": outcome.strategy.as_str(),
                    "source_storage_type": outcome.source_storage_label,
                }),
            ));
        }
        Err(e) => {
            out.errors = 1;
            out.had_error = true;
            let err_msg = format!("{}", e);
            {
                let db = db.lock().await;
                db.replication_record_object_failure(
                    rule_name,
                    src_key,
                    &err_msg,
                    current_unix_seconds(),
                )?;
            }
            log_failure(
                db,
                rule_name,
                run_id,
                src_key,
                dest_key,
                &err_msg,
                max_failures_retained,
            )
            .await?;
            debug!(
                "replication rule '{}' object failure src={:?} dst={:?}: {}",
                rule_name, src_key, dest_key, e
            );
            out.dest_fatal = is_destination_fatal(&err_msg);
        }
    }
    Ok(out)
}

/// True iff a copy error means the DESTINATION is fundamentally unusable for the
/// whole run — the dest bucket doesn't exist, the account is suspended, or the
/// backend is out of storage/quota. These are NOT per-object hiccups: no object
/// will ever land, so the run must FAIL FAST instead of retrying every object
/// against a dead destination (the prod case: a Backblaze bucket over quota was
/// surfacing `NoSuchBucket`/quota errors per-object and grinding through ~93K
/// objects every tick). Pure so the truth table is unit-tested.
fn is_destination_fatal(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    // Dest bucket missing / account suspended (StorageError::BucketNotFound's
    // Display is "Bucket not found: …"; Backblaze returns NoSuchBucket).
    e.contains("bucket not found")
        || e.contains("nosuchbucket")
        // Storage cap / quota exhaustion — provider-agnostic signatures.
        || e.contains("quota")
        || e.contains("insufficient storage")
        || e.contains("cap exceeded")
        || e.contains("storage limit")
}

/// Resolves to `true` once a kill is requested for `run_id` (polls the DB
/// `cancelling` flag ~1×/s). Used as the cancel arm of the per-page select —
/// when it wins, the page's copy future is dropped and in-flight transfers abort.
async fn poll_run_killed(db: &Arc<Mutex<ConfigDb>>, run_id: i64) -> bool {
    loop {
        {
            let db = db.lock().await;
            if db.replication_run_cancel_requested(run_id).unwrap_or(false) {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn spawn_lease_heartbeat(
    db: Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    lease: Option<RunLease>,
    lease_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let lease = lease?;
    let rule_name = rule_name.to_string();
    let heartbeat_secs = lease.heartbeat_secs.max(1) as u64;
    Some(tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(heartbeat_secs);
        let lock_wait = std::time::Duration::from_secs(2);
        loop {
            tokio::time::sleep(interval).await;
            // Lock-light retry: a slow worker-side DB hold shouldn't drop the
            // lease. A lock-acquire timeout is retried (up to 3×); only a renew
            // that returns false (the SQL guard says the lease genuinely
            // lapsed) is terminal. `>= now` anti-resurrection lives in the SQL.
            let mut renewed = false;
            for _ in 0..3 {
                match tokio::time::timeout(lock_wait, db.lock()).await {
                    // Renew result is terminal either way: true = renewed,
                    // false/err = genuinely lapsed → stop retrying.
                    Ok(db) => {
                        renewed = db
                            .replication_renew_lease(
                                &rule_name,
                                &lease.owner,
                                current_unix_seconds(),
                                lease.ttl_secs,
                            )
                            .unwrap_or(false);
                        break;
                    }
                    // Couldn't even acquire the lock in time — retry the window.
                    Err(_elapsed) => continue,
                }
            }
            if renewed {
                continue;
            }
            // Lost if renew said false, OR all retries failed to acquire lock.
            lease_alive.store(false, std::sync::atomic::Ordering::Release);
            warn!(
                "Replication lease heartbeat lost for rule '{}'; worker will stop before more work",
                rule_name
            );
            return;
        }
    }))
}

async fn renew_run_lease(
    db: &Arc<Mutex<ConfigDb>>,
    rule: &ReplicationRule,
    lease: Option<&RunLease>,
    lease_alive: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    run_id: i64,
    max_failures_retained: u32,
) -> Result<bool, crate::config_db::ConfigDbError> {
    let Some(lease) = lease else {
        return Ok(true);
    };
    if !lease_alive.load(std::sync::atomic::Ordering::Acquire) {
        return record_lost_lease(db, &rule.name, run_id, max_failures_retained).await;
    }
    let now = current_unix_seconds();
    let guard = db.lock().await;
    if guard.replication_renew_lease(&rule.name, &lease.owner, now, lease.ttl_secs)? {
        return Ok(true);
    }
    drop(guard);
    record_lost_lease(db, &rule.name, run_id, max_failures_retained).await
}

async fn record_lost_lease(
    db: &Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    run_id: i64,
    max_failures_retained: u32,
) -> Result<bool, crate::config_db::ConfigDbError> {
    log_failure(
        db,
        rule_name,
        run_id,
        "",
        "",
        "lost replication lease; stopping run before more work",
        max_failures_retained,
    )
    .await?;
    Ok(false)
}

/// Flush buffered copy events under a freshly-acquired DB lock, draining
/// `events`. Used on the lease-loss break path where there's no
/// already-held guard. A failure is logged, not propagated — event
/// append is non-critical (the copies themselves are durable).
async fn flush_page_events(db: &Arc<Mutex<ConfigDb>>, rule_name: &str, events: &mut Vec<NewEvent>) {
    if events.is_empty() {
        return;
    }
    let guard = db.lock().await;
    flush_page_events_locked(&guard, rule_name, events);
}

/// Flush buffered copy events through an already-held DB guard, draining
/// `events`. Batches the whole page into one `event_outbox_insert_many`
/// so a 10k-object run costs one insert per page instead of per object.
fn flush_page_events_locked(db: &ConfigDb, rule_name: &str, events: &mut Vec<NewEvent>) {
    if events.is_empty() {
        return;
    }
    let count = events.len();
    if let Err(err) = db.event_outbox_insert_many(events) {
        warn!(
            "replication rule '{}' could not append {} copy event(s): {}",
            rule_name, count, err
        );
    }
    events.clear();
}

async fn log_failure(
    db: &Arc<Mutex<ConfigDb>>,
    rule_name: &str,
    run_id: i64,
    source_key: &str,
    dest_key: &str,
    error_message: &str,
    max_failures_retained: u32,
) -> Result<(), crate::config_db::ConfigDbError> {
    let db = db.lock().await;
    db.replication_record_failure(
        rule_name,
        FailureInsert {
            run_id: Some(run_id),
            occurred_at: current_unix_seconds(),
            source_key,
            dest_key,
            error_message,
        },
        max_failures_retained,
    )
}

/// Delete-replication pass: paginate the destination prefix; for each
/// key that's NOT on source, delete it from destination.
///
/// The key check is HEAD-on-source (cheaper than re-listing). If the
/// HEAD succeeds the source has it → keep destination's copy. If the
/// HEAD returns NotFound → delete destination.
///
/// Other errors (network, AccessDenied) are recorded as failures and
/// the destination key is preserved. Better to leave an extra copy than
/// to false-delete on a transient.
async fn run_delete_pass(
    db: Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    run_id: i64,
    totals: &mut RunTotals,
    had_any_error: &mut bool,
    max_failures_retained: u32,
) -> Result<(), crate::config_db::ConfigDbError> {
    let cap = rule.batch_size.clamp(1, 10_000);
    let destination_prefix = normalize_prefix(&rule.destination.prefix);

    let mut pager = Pager::fresh();
    'pages: while let Some(page_idx) = pager.begin_page() {
        // metadata=true so user_metadata (carrying our provenance
        // marker, H2 fix) is populated in the listing — saves a
        // per-object HEAD round-trip.
        let page = match engine
            .list_objects(
                &rule.destination.bucket,
                &destination_prefix,
                None,
                cap,
                pager.token(),
                true,
            )
            .await
        {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "replication rule '{}' delete-pass list page {} failed: {}",
                    rule.name, page_idx, e
                );
                log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("delete-pass list dest failed: {}", e),
                    max_failures_retained,
                )
                .await?;
                totals.errors += 1;
                *had_any_error = true;
                return Ok(());
            }
        };

        for (dest_key, listed_meta) in &page.objects {
            // H2 fix: only consider deleting objects this rule wrote.
            // Each replicated copy carries `dg-replication-rule = <rule.name>`
            // in user_metadata (stamped by `copy_one`). If the listed
            // metadata is missing (LIST without metadata=true) or the
            // marker doesn't match, skip — never delete unrelated
            // objects, even if their key-after-prefix-rewrite happens
            // to be missing on source.
            //
            // The list call below already passes `metadata=true` so
            // user_metadata is populated. Defence in depth: if it's
            // empty, we HEAD to confirm before any delete.
            let has_marker_in_listing = listed_meta
                .user_metadata
                .get(REPLICATION_RULE_METADATA_KEY)
                .map(|v| v == &rule.name)
                .unwrap_or(false);

            let owned_by_this_rule = if has_marker_in_listing {
                true
            } else {
                // Listing didn't carry user-metadata (some backends
                // omit it). HEAD the object to be sure.
                match engine.head(&rule.destination.bucket, dest_key).await {
                    Ok(meta) => meta
                        .user_metadata
                        .get(REPLICATION_RULE_METADATA_KEY)
                        .map(|v| v == &rule.name)
                        .unwrap_or(false),
                    // HEAD failed — preserve. Better to leak a
                    // candidate than false-delete a foreign object.
                    Err(_) => false,
                }
            };

            if !owned_by_this_rule {
                debug!(
                    "replication rule '{}' delete-pass skip (no provenance marker): {:?}",
                    rule.name, dest_key
                );
                continue;
            }

            // Translate dest key back to its source counterpart.
            let src_key = match dest_to_source_key(rule, dest_key) {
                Some(k) => k,
                None => {
                    // Key sits outside the rule's destination-prefix
                    // (paranoid case: marker matched but prefix doesn't).
                    continue;
                }
            };

            // HEAD source. NotFound → delete destination (we wrote it,
            // it's still under our prefix, source no longer has the
            // key — this is a legitimate deletion to replicate).
            // Other errors → leave alone, log as failure.
            match engine.head(&rule.source.bucket, &src_key).await {
                Ok(_) => {
                    // Source still has it. Skip.
                }
                Err(e) => {
                    let s3_err: crate::api::S3Error = e.into();
                    if matches!(s3_err, crate::api::S3Error::NoSuchKey(_)) {
                        // Source missing → replicate the deletion.
                        match engine.delete(&rule.destination.bucket, dest_key).await {
                            Ok(_) => {
                                totals.objects_deleted += 1;
                            }
                            Err(de) => {
                                totals.errors += 1;
                                *had_any_error = true;
                                log_failure(
                                    &db,
                                    &rule.name,
                                    run_id,
                                    &src_key,
                                    dest_key,
                                    &format!("destination delete failed: {}", de),
                                    max_failures_retained,
                                )
                                .await?;
                            }
                        }
                    } else {
                        // Anything else: log & preserve. False-delete
                        // would be much worse than a leftover copy.
                        totals.errors += 1;
                        *had_any_error = true;
                        log_failure(
                            &db,
                            &rule.name,
                            run_id,
                            &src_key,
                            dest_key,
                            &format!("delete-pass head source failed: {}", s3_err),
                            max_failures_retained,
                        )
                        .await?;
                    }
                }
            }
        }

        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break 'pages;
        }
    }

    Ok(())
}

/// Translate a destination key back to its source-side counterpart by
/// reversing the prefix-rewrite the planner applies.
///
/// Returns `None` when the destination key doesn't start with the
/// rule's destination prefix (which means it's outside this rule's
/// jurisdiction; the delete pass leaves it alone).
fn dest_to_source_key(rule: &ReplicationRule, dest_key: &str) -> Option<String> {
    let dst_prefix = normalize_prefix(&rule.destination.prefix);
    let src_prefix = normalize_prefix(&rule.source.prefix);
    let dst_prefix = dst_prefix.as_str();
    let src_prefix = src_prefix.as_str();
    if dst_prefix.is_empty() && src_prefix.is_empty() {
        return Some(dest_key.to_string());
    }
    if dst_prefix == src_prefix {
        return Some(dest_key.to_string());
    }
    if dst_prefix.is_empty() {
        return Some(format!(
            "{}{}",
            src_prefix,
            dest_key.trim_start_matches('/')
        ));
    }
    let tail = dest_key.strip_prefix(dst_prefix)?;
    Some(format!("{}{}", src_prefix, tail.trim_start_matches('/')))
}

/// Compute when this rule should next be due. Falls back to a 1-hour
/// recovery window if the rule's `interval` is unparseable (should
/// never happen in practice — validated at Config::check time).
fn compute_next_due(rule: &ReplicationRule, finished_at: i64) -> i64 {
    match humantime::parse_duration(&rule.interval) {
        Ok(d) => finished_at + d.as_secs() as i64,
        Err(_) => finished_at + 3600,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::{ConflictPolicy, ReplicationEndpoint, ReplicationRule};

    #[test]
    fn destination_fatal_truth_table() {
        // Fatal: dest bucket gone / account suspended / over quota.
        for f in [
            "Bucket not found: beshu-b2",
            "S3 error: NoSuchBucket",
            "storage error: quota exceeded",
            "Insufficient storage",
            "B2 cap exceeded for account",
            "monthly storage limit reached",
        ] {
            assert!(is_destination_fatal(f), "expected fatal: {f}");
        }
        // Per-object hiccups: NOT fatal — keep going.
        for ok in [
            "object copy timed out after 1800s",
            "S3 error: put_object failed (status=503): SlowDown",
            "connection reset by peer",
            "NoSuchKey",
        ] {
            assert!(!is_destination_fatal(ok), "expected non-fatal: {ok}");
        }
    }

    fn mk_rule() -> ReplicationRule {
        ReplicationRule {
            name: "r".to_string(),
            enabled: true,
            source: ReplicationEndpoint {
                bucket: "a".into(),
                prefix: String::new(),
            },
            destination: ReplicationEndpoint {
                bucket: "b".into(),
                prefix: String::new(),
            },
            interval: "1h".into(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::NewerWins,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".into()],
        }
    }

    #[test]
    fn compute_next_due_honours_interval() {
        let rule = mk_rule();
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }

    #[test]
    fn compute_next_due_falls_back_on_invalid() {
        let mut rule = mk_rule();
        rule.interval = "garbage".into();
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }

    #[test]
    fn running_progress_updates_history_before_finish() {
        let db = ConfigDb::in_memory("testpass").unwrap();
        db.replication_ensure_state("r", 100).unwrap();
        let run_id = db.replication_begin_run("r", 100, "scheduler").unwrap();
        let totals = RunTotals {
            objects_scanned: 10,
            objects_copied: 4,
            objects_skipped: 6,
            objects_deleted: 0,
            bytes_copied: 1234,
            errors: 2,
            ..Default::default()
        };
        db.replication_update_run_progress(run_id, totals).unwrap();

        let runs = db.replication_recent_runs("r", 1).unwrap();
        assert_eq!(runs[0].status, "running");
        assert_eq!(runs[0].objects_scanned, 10);
        assert_eq!(runs[0].objects_copied, 4);
        assert_eq!(runs[0].errors, 2);
    }

    #[test]
    fn dest_to_source_key_identity_when_prefixes_empty() {
        let rule = mk_rule();
        assert_eq!(
            dest_to_source_key(&rule, "file.txt"),
            Some("file.txt".to_string())
        );
    }

    #[test]
    fn dest_to_source_key_strips_destination_prefix() {
        let mut rule = mk_rule();
        rule.source.prefix = "releases/".into();
        rule.destination.prefix = "archive/2026/".into();
        assert_eq!(
            dest_to_source_key(&rule, "archive/2026/v1.zip"),
            Some("releases/v1.zip".to_string())
        );
    }

    #[test]
    fn dest_to_source_key_returns_none_for_outside_keys() {
        let mut rule = mk_rule();
        rule.destination.prefix = "archive/".into();
        assert_eq!(dest_to_source_key(&rule, "other-stuff/x.bin"), None);
    }

    #[test]
    fn dest_to_source_key_handles_empty_dest_prefix_with_src_prefix() {
        let mut rule = mk_rule();
        rule.source.prefix = "releases/".into();
        rule.destination.prefix = "".into();
        assert_eq!(
            dest_to_source_key(&rule, "v1.zip"),
            Some("releases/v1.zip".to_string())
        );
    }
}
