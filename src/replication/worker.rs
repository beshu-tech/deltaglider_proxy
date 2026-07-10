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
use crate::config_sections::{ConflictPolicy, ReplicationRule};
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::job_loop::Pager;
use crate::metrics::{bump_peak, Metrics};
use crate::transfer::{
    copy_object_with_retries, CopyStrategy, ObjectTransferRequest, TransferProvenance,
    REPLICATION_RULE_METADATA_KEY,
};
use crate::types::FileMetadata;
use futures::stream::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// One object currently being copied — feeds the Jobs UI so a slow-moving
/// counter is explained ("copying big.tar.gz · 4.2 GB"). In-memory and
/// node-local by design: this is LIVE progress, not durable state (a restart
/// or kill simply clears it via the RAII guards).
#[derive(Clone, serde::Serialize)]
pub struct InFlightCopy {
    pub key: String,
    pub size: u64,
    pub started_unix: i64,
}

static INFLIGHT: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, Vec<InFlightCopy>>>,
> = std::sync::LazyLock::new(Default::default);

/// Snapshot of a rule's in-flight copies for the admin jobs API,
/// largest-first (the big file is the one that explains the wait).
pub fn inflight_snapshot(rule: &str) -> Vec<InFlightCopy> {
    let map = INFLIGHT.lock();
    let mut v = map.get(rule).cloned().unwrap_or_default();
    v.sort_by_key(|e| std::cmp::Reverse(e.size));
    v
}

/// RAII registration of one in-flight copy: inserted on construction, removed
/// on drop — a killed page (its collect future dropped) cleans up implicitly.
struct InFlightGuard {
    rule: String,
    key: String,
}

impl InFlightGuard {
    fn new(rule: &str, key: &str, size: u64) -> Self {
        INFLIGHT
            .lock()
            .entry(rule.to_string())
            .or_default()
            .push(InFlightCopy {
                key: key.to_string(),
                size,
                started_unix: current_unix_seconds(),
            });
        Self {
            rule: rule.to_string(),
            key: key.to_string(),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut map = INFLIGHT.lock();
        if let Some(v) = map.get_mut(&self.rule) {
            if let Some(pos) = v.iter().position(|e| e.key == self.key) {
                v.remove(pos);
            }
            if v.is_empty() {
                map.remove(&self.rule);
            }
        }
    }
}

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

/// Test-only per-object stall, called in BOTH the forward-copy pass (inside the
/// per-object timeout scope, so the object-timeout Elapsed arm is testable) AND
/// the delete pass (so a kill lands deterministically mid-delete). Sleeps
/// `DGP_TEST_COPY_STALL_MS` ms when set (>0) — the env var keeps its historical
/// name. Inert in prod (unset → no-op).
async fn maybe_pass_stall() {
    let ms: u64 = crate::config::env_parse_with_default("DGP_TEST_COPY_STALL_MS", 0);
    if ms > 0 {
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
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
    coordination_lease: Option<Arc<dyn crate::coordination::CoordinationLease>>,
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
    // Set only on a dest-unusable abort (dead bucket / over quota). Such a dest
    // won't recover in 60s, so we back off to the rule's normal cadence rather
    // than re-firing every minute and hammering the dead endpoint forever.
    let mut dest_unusable = false;
    // Set on a whole-page throttle abort (503 SlowDown): same backoff as
    // dest_unusable — a backend shedding load needs breathing room, not a
    // 60s retry hammer.
    let mut backend_throttled = false;
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
    let dest_prefix = normalize_prefix(&rule.destination.prefix);
    let lease_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat_handle = spawn_lease_heartbeat(
        db.clone(),
        &rule.name,
        lease.clone(),
        coordination_lease.clone(),
        lease_alive.clone(),
    );
    // A run-now is a deliberate ONE-OFF: it runs even a paused rule (pause
    // governs the scheduler); KILL is the stop affordance for a running one-off.
    let ctrl = RunControl {
        db: db.clone(),
        rule_name: rule.name.clone(),
        run_id,
        lease: lease.clone(),
        coordination_lease: coordination_lease.clone(),
        lease_alive: lease_alive.clone(),
        one_off: triggered_by == "run-now",
        max_failures_retained,
        dest_bucket: rule.destination.bucket.clone(),
        maintenance_gate,
    };
    let events_sink: EventSink = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));

    // Build a dest-presence ORACLE once per run by DESCENDING the dest+source
    // prefix trees with a `/` delimiter, instead of HEADing every source key (or
    // even flat-listing the whole dest). A HEAD per object against a remote dest
    // is what made a copy to an empty/sparse destination spend minutes "thinking"
    // before copying anything. With the oracle, an absent dest subtree is proven
    // from a single common-prefix probe — its objects all copy with no HEAD — and
    // planning HEADs a dest key ONLY when it actually exists. Bounded: an enormous
    // or errored/cancelled descent falls back to per-key HEAD (old behavior).
    let dest_oracle = build_dest_oracle(engine, &ctrl, rule, &dest_prefix, &source_prefix).await;

    let mut pager = Pager::resuming(continuation).with_test_max_pages_env();
    // Fold a DB error into a fatal STOP instead of `?`-returning out of
    // run_rule: the post-loop event flush + settle must ALWAYS run (an early
    // return leaves the run row 'running' forever and drops buffered events).
    macro_rules! fatal_break {
        ($lbl:lifetime, $msg:expr) => {{
            warn!("replication rule '{}': {}", rule.name, $msg);
            totals.errors += 1;
            hit_fatal_error = true;
            break $lbl;
        }};
    }
    // ── Forward-copy pass: paginate source until exhausted ──
    'pages: while let Some(page_idx) = pager.begin_page() {
        // Uniform page-boundary control check (kill / pause / lease renew).
        // ponytail: PAUSE is a live DB flag; DISABLE is a config edit the worker
        // can't see through its `&ReplicationRule` snapshot — Pause is the button.
        match ctrl.check(true).await {
            Err(e) => fatal_break!('pages, format!("control check failed: {e}")),
            Ok(v) => match v {
                ControlVerdict::Continue => {}
                ControlVerdict::Killed => {
                    info!(
                        "replication rule '{}' killed mid-run — stopping at page {} boundary",
                        rule.name, page_idx
                    );
                    killed = true;
                    break 'pages;
                }
                ControlVerdict::Paused => {
                    info!(
                        "replication rule '{}' paused mid-run — stopping after page {} (cursor preserved for resume)",
                        rule.name, page_idx
                    );
                    stopped_paused = true;
                    break 'pages;
                }
                ControlVerdict::LeaseLost => {
                    totals.errors += 1;
                    hit_fatal_error = true;
                    break 'pages;
                }
            },
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
                if let Err(le) = log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("list source failed: {}", e),
                    max_failures_retained,
                )
                .await
                {
                    warn!(
                        "replication rule '{}': failure-ring write failed: {le}",
                        rule.name
                    );
                }
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
            let dest_oracle = &dest_oracle;
            let conflict = rule.conflict;
            plan_batch(&page.objects, rule, move |dest_key| {
                let engine = head_engine.clone();
                let dest_bucket = dest_bucket.clone();
                let dk = dest_key.to_string();
                // Decide what dest metadata (if any) the planner needs WITHOUT a
                // HEAD where provable:
                //  - absent on dest   → None (copy under every policy).
                //  - SkipIfDestExists → existence ONLY; the policy discards the
                //    metadata, so a synth meta (any) skips the HEAD. Safe on every
                //    backend incl. encrypted (we never read the dest's content).
                //  - else (ContentDiff / NewerWins / delta-eligible) → real HEAD.
                //    NOTE: ContentDiff head-free from the lite list is UNSAFE on an
                //    encrypting dest backend — the lite list returns ciphertext
                //    size/etag while a HEAD returns decrypted logical facts, so a
                //    lite compare would over-copy every tick. Deferred until the
                //    engine can cheaply report whether a dest bucket encrypts.
                let maybe_present = dest_oracle.may_contain(&dk);
                let synth = if !maybe_present {
                    Some(None) // resolved: absent
                } else if let Some(leaf) = dest_oracle.leaf(&dk) {
                    if matches!(conflict, ConflictPolicy::SkipIfDestExists) {
                        Some(Some(leaf.synth_meta(&dk)))
                    } else {
                        None // need a real HEAD (decrypted/logical facts)
                    }
                } else {
                    None // Unbounded / no leaf — HEAD
                };
                async move {
                    match synth {
                        Some(resolved) => resolved,
                        None => engine.head(&dest_bucket, &dk).await.ok(),
                    }
                }
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
                if let Err(le) = log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    &format!("planner error: {}", e),
                    max_failures_retained,
                )
                .await
                {
                    warn!(
                        "replication rule '{}': failure-ring write failed: {le}",
                        rule.name
                    );
                }
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        };

        totals.objects_skipped += plan.skipped.len() as i64;

        // Register this page's dest writes with the maintenance gate BEFORE the
        // busy check, so the acquire-then-recheck closes the TOCTOU with a
        // maintenance job arming concurrently (H22): either our +1 is visible to
        // its drain_inflight_writes (it waits for this page), or ctrl.check below
        // sees is_busy and we defer with the guard dropped and no copies done.
        // The per-page barrier bounds how long a drain can wait on us.
        let _page_write = ctrl
            .maintenance_gate
            .as_ref()
            .map(|g| g.begin_write(&ctrl.dest_bucket));

        // Renew the lease ONCE before the page's concurrent copy batch.
        // The independent heartbeat task keeps it alive during the batch;
        // the post-batch check re-reads it. Events flush post-loop.
        match ctrl.check(true).await {
            Err(e) => fatal_break!('pages, format!("control check failed: {e}")),
            Ok(ControlVerdict::Continue) => {}
            Ok(ControlVerdict::Killed) => {
                killed = true;
                break 'pages;
            }
            Ok(ControlVerdict::Paused) => {
                stopped_paused = true;
                break 'pages;
            }
            Ok(ControlVerdict::LeaseLost) => {
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
        }

        // Copy up to `transfers` objects concurrently. Each unit does its
        // own DB writes (failure/clear — they serialize through the shared
        // Arc<Mutex<ConfigDb>>) and returns its totals delta + optional
        // event. The page boundary is the barrier: the cursor does not
        // advance until every in-flight object of this page finishes.
        let page_attempted = plan.to_copy.len() as i64;
        let copy_page = futures::stream::iter(plan.to_copy.clone())
            .map(|(src_key, dest_key, src_size)| {
                let db = db.clone();
                let engine = engine.clone();
                let rule_name = rule.name.clone();
                let src_bucket = rule.source.bucket.clone();
                let dst_bucket = rule.destination.bucket.clone();
                let events = events_sink.clone();
                async move {
                    // Guard increments objects_inflight (+peak) on entry and
                    // decrements on drop → proves the `transfers` concurrency.
                    let _obj_guard = engine.metrics().cloned().map(ObjectGuard::new);
                    // Live "currently copying" registration for the Jobs UI
                    // (RAII: a kill dropping this future unregisters it).
                    let _inflight = InFlightGuard::new(&rule_name, &src_key, src_size);
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
                        &events,
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
        let mut page_copied: i64 = 0;
        let mut page_throttled: i64 = 0;
        let mut clear_failure_keys: Vec<String> = Vec::new();
        for res in object_results {
            let res = match res {
                Ok(r) => r,
                Err(e) => fatal_break!('pages, format!("per-object DB write failed: {e}")),
            };
            if let Some(k) = res.clear_failure_key {
                clear_failure_keys.push(k);
            }
            totals.objects_copied += res.objects_copied;
            totals.objects_skipped += res.objects_skipped;
            totals.bytes_copied += res.bytes_copied;
            totals.errors += res.errors;
            totals.delta_passthrough += res.delta_passthrough;
            totals.bytes_egress_saved += res.bytes_egress_saved;
            page_copied += res.objects_copied;
            if res.had_error {
                had_any_error = true;
            }
            if res.dest_fatal {
                dest_fatal = true;
            }
            if res.throttled {
                page_throttled += 1;
            }
        }
        // Destination unusable (bucket missing / over quota) — abort the run
        // instead of retrying every remaining object against a dead dest.
        // GATE on zero successes this page: a single object's stray token
        // ("quota" in an IAM denial, a key echoing "storage limit") must NOT
        // abort a page that is otherwise copying fine. A truly dead dest fails
        // EVERY object, so page_copied==0 holds for the real case.
        if dest_fatal && page_copied == 0 {
            warn!(
                "replication rule '{}' aborting run: destination unusable (bucket missing or over quota)",
                rule.name
            );
            hit_fatal_error = true;
            dest_unusable = true;
            break 'pages;
        }
        // Backend shedding load (503 SlowDown / 429): abort instead of
        // grinding the remaining key list through per-object retries. Gated
        // on zero successes this page AND (≥3 throttled OR the WHOLE page
        // throttled). The whole-page clause matters for small batch_size (≤2):
        // page_throttled>=3 alone is unsatisfiable there, so a fully-throttling
        // backend would grind object-by-object with no backoff (the exact prod
        // incident the gate exists for). The cursor resumes after backoff;
        // copies are idempotent. Guard page_attempted>0 so an empty page (all
        // filtered) never trips it.
        if page_is_throttle_aborted(page_copied, page_throttled, page_attempted) {
            warn!(
                "replication rule '{}' aborting run: backend throttled ({} objects rejected with SlowDown this page)",
                rule.name, page_throttled
            );
            let _ = log_failure(
                &db,
                &rule.name,
                run_id,
                "",
                "",
                &format!(
                    "run aborted: backend throttled ({page_throttled} SlowDown rejections in one page); \
                     resuming from cursor after backoff"
                ),
                max_failures_retained,
            )
            .await;
            hit_fatal_error = true;
            backend_throttled = true;
            break 'pages;
        }
        {
            let db = db.lock().await;
            if let Err(e) = db.replication_update_run_progress(run_id, totals) {
                drop(db);
                fatal_break!('pages, format!("progress persist failed: {e}"));
            }
        }

        // Post-batch control re-check: stop before persisting the cursor if a
        // kill/pause/lease-loss landed while the batch ran (no renew: the
        // heartbeat owns liveness during the batch).
        match ctrl.check(false).await {
            Err(e) => fatal_break!('pages, format!("control check failed: {e}")),
            Ok(ControlVerdict::Continue) => {}
            Ok(ControlVerdict::Killed) => {
                killed = true;
                break 'pages;
            }
            Ok(ControlVerdict::Paused) => {
                stopped_paused = true;
                break 'pages;
            }
            Ok(ControlVerdict::LeaseLost) => {
                // First detection (check(false) never logs): record it so the
                // failure ring explains the failed run.
                let _ = log_failure(
                    &db,
                    &rule.name,
                    run_id,
                    "",
                    "",
                    "lost replication lease; stopping run before more work",
                    max_failures_retained,
                )
                .await;
                totals.errors += 1;
                hit_fatal_error = true;
                break 'pages;
            }
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
            // Clear the failure ledger for every durably-copied object in this
            // page (deferred from copy_one_object so a mid-object kill can't lose
            // it — H15). Best-effort: a clear failure must not abort the run.
            for k in &clear_failure_keys {
                let _ = db.replication_clear_object_failure(&rule.name, k);
            }
            let persist = db
                .replication_set_continuation_token(&rule.name, pager.token())
                .and_then(|_| db.replication_update_run_progress(run_id, totals));
            let mut drained: Vec<NewEvent> = std::mem::take(&mut *events_sink.lock());
            flush_page_events_locked(&db, &rule.name, &mut drained);
            if let Err(e) = persist {
                drop(db);
                fatal_break!('pages, format!("cursor/progress persist failed: {e}"));
            }
        }

        if !more {
            break 'pages;
        }
    }

    // Unconditional flush: covers EVERY break path (kill, pause, lease,
    // dest-fatal) — events pushed by durably-completed copies must survive.
    flush_event_sink(&db, &rule.name, &events_sink).await;
    // Forward pass ran out of page budget with pages pending: the cursor
    // stays persisted so the next tick resumes the tail (never a clean pass).
    let truncated = pager.truncated_by_page_budget();

    // ── Delete-replication pass (opt-in per rule) ──
    //
    // After the forward copy completes, paginate the destination prefix
    // and delete every key whose corresponding source key is missing.
    // Only fires on a COMPLETE clean forward pass — a partial/truncated
    // listing could make a present source key look missing, and a false
    // destination wipe would be catastrophic.
    if rule.replicate_deletes && !hit_fatal_error && !stopped_paused && !killed && !truncated {
        match run_delete_pass(
            &ctrl,
            engine,
            rule,
            run_id,
            &mut totals,
            &mut had_any_error,
            max_failures_retained,
        )
        .await
        {
            Ok(ControlVerdict::Continue) => {}
            Ok(ControlVerdict::Killed) => killed = true,
            Ok(ControlVerdict::Paused) => stopped_paused = true,
            Ok(ControlVerdict::LeaseLost) => {
                totals.errors += 1;
                hit_fatal_error = true;
            }
            Err(e) => {
                warn!("replication rule '{}' delete pass error: {}", rule.name, e);
                had_any_error = true;
            }
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
    // Final authoritative kill check: a kill requested while the LAST page was
    // draining wouldn't have flipped `killed` (the select! race resolved to the
    // results arm). The DB `cancelling` row is the source of truth — honor it so
    // the operator's kill is never silently overwritten by a success status.
    let finished_at = current_unix_seconds();
    // Settle under ONE lock: re-read the DB cancel flag, derive the terminal
    // status, and write finish_run without releasing in between (H1 fix); the
    // finish UPDATE is ALSO guarded `status IN ('running','cancelling')` in SQL.
    // Errors inside settle must NOT skip the heartbeat abort below — capture,
    // abort, then propagate.
    let settle_result: Result<String, crate::config_db::ConfigDbError> = async {
        let db = db.lock().await;
        // Authoritative final kill check (see the select! race comment above):
        // the DB `cancelling` row is the source of truth for a kill that arrived
        // while the last page drained.
        if !killed && db.replication_run_cancel_requested(run_id).unwrap_or(false) {
            killed = true;
        }

        let decision = settle_run(SettleInput {
            killed,
            stopped_paused,
            hit_fatal_error,
            had_any_error,
            objects_copied: totals.objects_copied,
            truncated,
        });

        let next_due = if dest_unusable || backend_throttled {
            // Dead dest won't recover in a minute, and a throttling backend
            // needs breathing room — back off to the rule's normal cadence
            // (but never faster than 60s) instead of hammering every minute.
            compute_next_due(rule, finished_at).max(finished_at + 60)
        } else if hit_fatal_error || truncated {
            // Tight retry on fatal errors AND budget truncation: the persisted
            // cursor resumes the tail promptly instead of waiting a full cadence.
            finished_at + 60
        } else {
            compute_next_due(rule, finished_at)
        };

        if decision.clear_cursor {
            db.replication_set_continuation_token(&rule.name, None)?;
        }
        if !db.replication_finish_run(
            run_id,
            &rule.name,
            decision.status,
            finished_at,
            totals,
            next_due,
        )? {
            warn!(
                "replication rule '{}' run {} already terminal; settle to '{}' skipped",
                rule.name, run_id, decision.status
            );
        }
        Ok(decision.status.to_string())
    }
    .await;
    // Settle barrier: bump AFTER the terminal row is written so a test polling
    // the run-version sees the settled run. The single chokepoint all scheduled
    // runs pass through.
    super::state_store::bump_replication_run_version();
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }
    let status = settle_result?;

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
    // The error was a backend throttle (503 SlowDown / 429): the caller
    // aborts the run when a whole page throttles instead of grinding the
    // remaining key list against a backend that is shedding load.
    throttled: bool,
    // Events go straight to the run's EventSink (not through this result):
    // a kill dropping the page's collect future must not lose them.
    // Fast-path attribution for the successful copy (zero otherwise).
    delta_passthrough: i64,
    bytes_egress_saved: i64,
    // On a DURABLE success, the source key whose failure ledger must be cleared.
    // Deferred to the page's post-copy DB critical section (NOT cleared inside
    // copy_one_object) so a kill dropping the collect future while an object is
    // parked on db.lock().await can't lose the clear — which would leave a stale
    // consecutive-failure count that later poison-skips a healthy object (H15).
    clear_failure_key: Option<String>,
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
    events: &EventSink,
) -> Result<PerObjectResult, crate::config_db::ConfigDbError> {
    let mut out = PerObjectResult::default();

    // Test-only barrier: force >=transfers objects co-resident (inert in prod).
    maybe_object_barrier().await;

    // Poison-object guard: skip an object that has failed every run for
    // `object_skip_after_failures` consecutive runs. Reset on success below.
    if object_skip_after_failures > 0 {
        let skipped = {
            let db = db.lock().await;
            db.replication_object_skipped(
                rule_name,
                src_key,
                object_skip_after_failures,
                current_unix_seconds(),
            )?
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
    let copy_fut = async {
        // Test-only stall INSIDE the timeout scope (inert in prod) so the
        // object-timeout Elapsed arm is deterministically exercisable.
        maybe_pass_stall().await;
        copy_object_with_retries(engine, transfer).await
    };
    let copy_result = match object_timeout {
        Some(timeout) => match tokio::time::timeout(timeout, copy_fut).await {
            Ok(r) => r,
            Err(_elapsed) => {
                Err(format!("object copy timed out after {}s", timeout.as_secs()).into())
            }
        },
        None => copy_fut.await,
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
            // Push the event BEFORE any further await: the copy is durable, so
            // a kill dropping this future from here on must not lose it.
            events.lock().push(NewEvent::new(
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
            // Defer the failure-ledger clear to the page's post-copy DB batch
            // (durable against a mid-object kill — see clear_failure_key).
            out.clear_failure_key = Some(src_key.to_string());
        }
        Err(e) => {
            out.errors = 1;
            out.had_error = true;
            let err_msg = format!("{}", e);
            // A backend THROTTLE is not an object-specific fault — do not count
            // it toward the poison-skip ledger, or a throttling backend would
            // mass-poison a whole page of healthy objects into permanent skip.
            // (The page-level throttle-abort already stops the run.)
            out.throttled = is_backend_throttled(&err_msg);
            if !out.throttled {
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
            // out.throttled already set above (gates the poison-ledger record).
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
///
/// This answers "COULD this error mean a dead dest" — some tokens (quota,
/// access denied) can also be per-object. The caller's page-level zero-success
/// gate makes that safe: a dead dest fails every object, so the abort only fires
/// when nothing copied. (A fuller fix would classify on the typed StorageError
/// variant before the retry loop — deferred; substring + the page gate closes
/// the prod incident without re-plumbing the engine error type.)
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
        // Real Backblaze B2 over-cap shapes (machine code + human text + the
        // 403-disabled message). The page-level zero-success gate in the fold
        // makes these safe to match even though they CAN appear per-object.
        || e.contains("cap_exceeded")
        || e.contains("exceed account cap")
        || e.contains("account cap")
        || e.contains("all access to this object has been disabled")
        // Dead/wrong destination credentials or endpoint — no object will land.
        || e.contains("accessdenied")
        || e.contains("access denied")
        || e.contains("signaturedoesnotmatch")
        || e.contains("permanentredirect")
}

/// True iff a copy error is a backend THROTTLE signal (Ceph/AWS `503
/// SlowDown`, generic 429). Like `is_destination_fatal` this answers "COULD
/// this be a throttle" — the caller's page-level gate (several throttled
/// objects AND zero successes) keeps a stray token from aborting a healthy
/// run. Pure so the truth table is unit-tested.
fn is_backend_throttled(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("slowdown")
        || e.contains("throttl") // "throttled"/"throttling" (StorageError::Throttled Display)
        || e.contains("too many requests")
        || e.contains("status=503")
        || e.contains("(503")
}

/// Pure: should the run abort this page as backend-throttled? True with ZERO
/// successes AND (≥3 throttled OR the whole page throttled). The whole-page
/// clause covers small batch_size (≤2) where `>= 3` is unsatisfiable (H16); the
/// ≥3 clause keeps a couple of stray 503s from aborting a large otherwise-idle
/// page. `attempted == 0` can't trip it (no throttles without attempts).
fn page_is_throttle_aborted(copied: i64, throttled: i64, attempted: i64) -> bool {
    copied == 0 && throttled > 0 && (throttled >= 3 || throttled >= attempted)
}

/// Resolves to `true` once a kill is requested for `run_id` (polls the DB
/// `cancelling` flag ~1×/s). Used as the cancel arm of the per-page select —
/// when it wins, the page's copy future is dropped and in-flight transfers abort.
// ponytail: 1s poll → ≤1s kill latency. A notify channel would be tighter but
// the run loop has no other reason to hold one; poll until that changes.
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
    coordination_lease: Option<Arc<dyn crate::coordination::CoordinationLease>>,
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
            let renewed = if let Some(cl) = &coordination_lease {
                // Shared/cross-instance renew — matches the trait-based acquire.
                // The trait impl already carries its own bounded transient retry
                // (S3Lease) or is a single SQLite CAS (LocalLease). A false/err
                // verdict is terminal (genuinely lapsed / stolen).
                cl.renew(
                    crate::coordination::LeaseSubsystem::Replication,
                    &rule_name,
                    &lease.owner,
                    current_unix_seconds(),
                    lease.ttl_secs,
                )
                .await
                .unwrap_or(false)
            } else {
                // Node-local SQLite renew with lock-light retry: a slow worker-side
                // DB hold shouldn't drop the lease. Lock-acquire timeout retried
                // (up to 3×); only a renew returning false is terminal.
                let mut ok = false;
                for _ in 0..3 {
                    match tokio::time::timeout(lock_wait, db.lock()).await {
                        Ok(db) => {
                            ok = db
                                .replication_renew_lease(
                                    &rule_name,
                                    &lease.owner,
                                    current_unix_seconds(),
                                    lease.ttl_secs,
                                )
                                .unwrap_or(false);
                            break;
                        }
                        Err(_elapsed) => continue,
                    }
                }
                ok
            };
            if renewed {
                continue;
            }
            lease_alive.store(false, std::sync::atomic::Ordering::Release);
            warn!(
                "Replication lease heartbeat lost for rule '{}'; worker will stop before more work",
                rule_name
            );
            return;
        }
    }))
}

/// Inputs to the PURE terminal-settle decision (see `settle_run`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettleInput {
    killed: bool,
    stopped_paused: bool,
    hit_fatal_error: bool,
    had_any_error: bool,
    objects_copied: i64,
    /// Forward pass ran out of page budget with pages still pending.
    truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SettleDecision {
    status: &'static str,
    clear_cursor: bool,
}

/// Terminal status + cursor decision, pure so the truth table is unit-tested.
/// Precedence: killed > paused > fatal > budget-truncation ("stopped": halted
/// mid-sweep, cursor kept, next tick resumes the tail) > errors > clean.
/// The cursor survives ANY interrupted/truncated pass — clearing it on a
/// truncated "clean" run permanently orphaned objects past the page budget.
fn settle_run(i: SettleInput) -> SettleDecision {
    let status = if i.killed {
        "cancelled"
    } else if i.stopped_paused {
        "stopped"
    } else if i.hit_fatal_error || (i.had_any_error && i.objects_copied == 0) {
        "failed"
    } else if i.truncated {
        "stopped"
    } else if i.had_any_error {
        "completed_with_errors"
    } else {
        "succeeded"
    };
    let clear_cursor = !i.killed && !i.stopped_paused && !i.hit_fatal_error && !i.truncated;
    SettleDecision {
        status,
        clear_cursor,
    }
}

/// What a run-control check decided. Precedence: Killed > LeaseLost > Paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlVerdict {
    Continue,
    Killed,
    Paused,
    LeaseLost,
}

/// PURE precedence fold for the page-boundary control check. `one_off`
/// (run-now) suppresses ONLY the pause — kill and lease always apply.
fn control_verdict(
    cancel_requested: bool,
    paused: bool,
    one_off: bool,
    lease_ok: bool,
) -> ControlVerdict {
    if cancel_requested {
        ControlVerdict::Killed
    } else if !lease_ok {
        ControlVerdict::LeaseLost
    } else if paused && !one_off {
        ControlVerdict::Paused
    } else {
        ControlVerdict::Continue
    }
}

/// PURE: `control_verdict` plus the mid-run maintenance overlay. A dest bucket
/// that went write-gated mid-run defers like a pause (cursor kept) — but ONLY
/// when nothing higher-precedence fired: a kill or a lost lease MUST still win
/// (we're stopping regardless; the reason matters for status/settle). `one_off`
/// does NOT suppress the maintenance defer (a one-off must not write into a
/// bucket being rewritten either).
fn control_verdict_with_maintenance(
    cancel_requested: bool,
    paused: bool,
    one_off: bool,
    lease_ok: bool,
    maint_busy: bool,
) -> ControlVerdict {
    let base = control_verdict(cancel_requested, paused, one_off, lease_ok);
    if base == ControlVerdict::Continue && maint_busy {
        ControlVerdict::Paused
    } else {
        base
    }
}

/// Uniform run-control: kill / pause / lease evaluated identically at every
/// page boundary of every pass (forward copy, delete pass, oracle descent).
struct RunControl {
    db: Arc<Mutex<ConfigDb>>,
    rule_name: String,
    run_id: i64,
    lease: Option<RunLease>,
    /// Cross-instance lease (when the job plane runs shared). When present the
    /// heartbeat + control renew go through it (matching the acquire), so an
    /// S3-CAS lease is renewed against the SAME object the scheduler took — not
    /// the node-local SQLite row.
    coordination_lease: Option<Arc<dyn crate::coordination::CoordinationLease>>,
    lease_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    one_off: bool,
    max_failures_retained: u32,
    /// Destination bucket + the write-gate: if a maintenance job starts
    /// rewriting the dest MID-RUN, defer (stop, cursor preserved) instead of
    /// writing into a bucket being migrated/re-encrypted (finding #12).
    dest_bucket: String,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
}

impl RunControl {
    /// One db.lock: cancel flag + paused flag + (when `renew`) lease renewal.
    /// A lost lease is recorded as a run failure only on the `renew` variant
    /// so back-to-back checks don't double-log.
    async fn check(&self, renew: bool) -> Result<ControlVerdict, crate::config_db::ConfigDbError> {
        let mut lease_ok =
            self.lease.is_none() || self.lease_alive.load(std::sync::atomic::Ordering::Acquire);
        // Read cancel/paused under the DB lock, then DROP it before any lease
        // renew — the trait renew may do S3 I/O and must never run while holding
        // the global config-DB mutex.
        let (cancel_requested, paused) = {
            let g = self.db.lock().await;
            let cancel = g
                .replication_run_cancel_requested(self.run_id)
                .unwrap_or(false);
            let paused = matches!(
                g.replication_load_state(&self.rule_name),
                Ok(Some(st)) if st.paused
            );
            (cancel, paused)
        };
        if lease_ok && renew {
            if let (Some(l), Some(cl)) = (&self.lease, &self.coordination_lease) {
                // Shared/cross-instance renew — matches the trait-based acquire.
                lease_ok = cl
                    .renew(
                        crate::coordination::LeaseSubsystem::Replication,
                        &self.rule_name,
                        &l.owner,
                        current_unix_seconds(),
                        l.ttl_secs,
                    )
                    .await
                    .unwrap_or(false);
            } else if let Some(l) = &self.lease {
                // No injected coordination lease (e.g. an admin run-now without
                // one) → the node-local SQLite renew, as before.
                let g = self.db.lock().await;
                lease_ok = g.replication_renew_lease(
                    &self.rule_name,
                    &l.owner,
                    current_unix_seconds(),
                    l.ttl_secs,
                )?;
            }
        }
        // Mid-run maintenance deferral: a dest bucket that became write-gated
        // (migrate / re-encrypt) must stop the run like a pause — cursor kept,
        // resumes when the maintenance job clears. Not suppressed by one_off:
        // a one-off must NOT write into a bucket being rewritten either.
        let maint_busy = self
            .maintenance_gate
            .as_ref()
            .map(|g| g.is_busy(&self.dest_bucket))
            .unwrap_or(false);
        let verdict = control_verdict_with_maintenance(
            cancel_requested,
            paused,
            self.one_off,
            lease_ok,
            maint_busy,
        );
        if verdict == ControlVerdict::LeaseLost && renew {
            log_failure(
                &self.db,
                &self.rule_name,
                self.run_id,
                "",
                "",
                "lost replication lease; stopping run before more work",
                self.max_failures_retained,
            )
            .await?;
        }
        Ok(verdict)
    }
}

/// Per-run event sink: copy futures push events the moment a copy is durable,
/// so a kill that drops the page's collect future can't lose them.
type EventSink = std::sync::Arc<parking_lot::Mutex<Vec<NewEvent>>>;

/// Drain the sink and flush under a freshly-acquired DB lock.
async fn flush_event_sink(db: &Arc<Mutex<ConfigDb>>, rule_name: &str, sink: &EventSink) {
    let mut drained: Vec<NewEvent> = std::mem::take(&mut *sink.lock());
    flush_page_events(db, rule_name, &mut drained).await;
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
    ctrl: &RunControl,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    run_id: i64,
    totals: &mut RunTotals,
    had_any_error: &mut bool,
    max_failures_retained: u32,
) -> Result<ControlVerdict, crate::config_db::ConfigDbError> {
    let db = ctrl.db.clone();
    let cap = rule.batch_size.clamp(1, 10_000);
    let destination_prefix = normalize_prefix(&rule.destination.prefix);

    let mut pager = Pager::fresh().with_test_max_pages_env();
    'pages: while let Some(page_idx) = pager.begin_page() {
        // This is the run's only DESTRUCTIVE phase — the same page-boundary
        // control check as the forward pass: a killed run must stop deleting.
        match ctrl.check(true).await? {
            ControlVerdict::Continue => {}
            verdict => {
                info!(
                    "replication rule '{}' delete pass stopping at page {} ({:?})",
                    rule.name, page_idx, verdict
                );
                return Ok(verdict);
            }
        }
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
                return Ok(ControlVerdict::Continue);
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
                        // Same test-only stall as the copy path: lets a kill land
                        // deterministically mid-delete-pass (inert in prod).
                        maybe_pass_stall().await;
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

    // Budget truncation: the dest tail was never scanned. Surface it (safe
    // direction — under-delete) instead of pretending the sweep completed.
    if pager.truncated_by_page_budget() {
        log_failure(
            &db,
            &rule.name,
            run_id,
            "",
            "",
            "delete pass truncated by page budget; destination tail not swept this run",
            max_failures_retained,
        )
        .await?;
        totals.errors += 1;
        *had_any_error = true;
    }

    Ok(ControlVerdict::Continue)
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

/// Minimal lite facts about a present destination object, captured from the
/// delimiter listing so policy compares (NewerWins timestamp, ContentDiff
/// size/etag) can run HEAD-free for passthrough keys (see the planner closure).
#[derive(Clone, Debug, PartialEq, Eq)]
struct DestLeaf {
    file_size: u64,
    /// Stored ETag from the lite list (md5 for passthrough, delta-blob etag for
    /// delta objects). Empty string ⇒ none.
    etag: String,
    created_at: i64,
}

impl DestLeaf {
    /// Synthesize a passthrough-shaped `FileMetadata` from the lite facts, so
    /// `should_replicate` can run a HEAD-free compare (Case 2/4). `file_sha256`
    /// is empty (the lite list never carries it) — ContentDiff then compares on
    /// size + etag, exactly as it does for any foreign object missing a SHA.
    fn synth_meta(&self, dest_key: &str) -> FileMetadata {
        use chrono::TimeZone;
        let name = dest_key.rsplit('/').next().unwrap_or(dest_key).to_string();
        let created = chrono::Utc
            .timestamp_millis_opt(self.created_at)
            .single()
            .unwrap_or_else(chrono::Utc::now);
        FileMetadata::fallback(
            name,
            self.file_size,
            self.etag.clone(),
            created,
            None,
            crate::types::StorageInfo::Passthrough,
        )
    }
}

/// Prefix-tree destination oracle, built once per run by DESCENDING the dest (and
/// source) trees with a `/` delimiter instead of flat-listing every key. Answers
/// the same `may_contain(dest_key)` question the old flat `DestIndex` did, but a
/// whole dest subtree that doesn't exist is proven absent from a single
/// common-prefix probe — so an empty/sparse dest costs near-zero dest work and a
/// missing subtree's objects all copy with no per-object HEAD.
enum DestOracle {
    Known {
        /// Dest keys that EXIST, with their lite facts (under existing subtrees).
        present: std::collections::HashMap<String, DestLeaf>,
        /// Normalized `pfx/` subtrees proven to have ZERO keys on the dest.
        absent_subtrees: Vec<String>,
    },
    /// Dest too large / listing errored / build cancelled — HEAD every key
    /// (identical to the pre-oracle behavior). NEVER a partial `Known`.
    Unbounded,
}

/// PURE: is `key` under the proven-absent subtree `prefix`? A `/`-terminated
/// prefix matches only true path descendants (`foo/` matches `foo/x`, NOT
/// `foobar/x`). An empty prefix (whole-bucket absent) matches everything. A
/// non-`/`-terminated prefix (shouldn't happen — common-prefixes always end in
/// `/`) is rejected defensively so it can never false-match a sibling.
fn key_under_absent_prefix(key: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    prefix.ends_with('/') && key.starts_with(prefix)
}

impl DestOracle {
    /// True if the key MIGHT exist on the destination (so a HEAD is warranted).
    /// `Unbounded` always returns true (preserves the HEAD-every-key fallback).
    /// A key under a proven-absent subtree short-circuits to false (copy, no HEAD).
    fn may_contain(&self, dest_key: &str) -> bool {
        match self {
            DestOracle::Unbounded => true,
            DestOracle::Known {
                present,
                absent_subtrees,
            } => {
                // `key_under_absent_prefix` requires the prefix to be `/`-terminated
                // so a sibling `foobar/x` can NEVER match the absent subtree `foo/`
                // (a raw `starts_with` would). Marking a live sibling absent →
                // copy-with-no-HEAD → overwrite, so this boundary is load-bearing.
                if absent_subtrees
                    .iter()
                    .any(|p| key_under_absent_prefix(dest_key, p))
                {
                    return false;
                }
                present.contains_key(dest_key)
            }
        }
    }

    /// Lite facts for a present dest key, if known (HEAD-free policy compare).
    fn leaf(&self, dest_key: &str) -> Option<&DestLeaf> {
        match self {
            DestOracle::Unbounded => None,
            DestOracle::Known { present, .. } => present.get(dest_key),
        }
    }
}

/// Cap on how many dest keys we'll hold in `present`. Above this the oracle
/// degrades to `Unbounded` (per-key HEAD) to bound memory. 1M keys ≈ tens of MB.
const DEST_INDEX_MAX_KEYS: usize = 1_000_000;
/// Cap on list calls during the descent (frontier pops). Bounds a pathological
/// deeply-nested tree; breach ⇒ `Unbounded`.
const MAX_ORACLE_LEVELS: usize = 50_000;
/// Cap on queued + recorded subtrees; breach ⇒ `Unbounded`.
const MAX_ORACLE_FRONTIER: usize = 100_000;
const MAX_ABSENT_SUBTREES: usize = 100_000;
/// Per-level page size for the delimiter listing (matches the old flat builder).
const LEVEL_PAGE_KEYS: u32 = 1000;

/// One level's pure classification: given the source and dest listings AT a
/// common prefix (objects + child common-prefixes), decide what to record. This
/// is the testable heart of the descent — no I/O.
struct LevelOutcome {
    /// Dest leaf objects present at this level → (dest_key, facts).
    present: Vec<(String, DestLeaf)>,
    /// Dest child subtrees to descend into (they exist on dest) → dest prefix.
    descend: Vec<String>,
    /// Source child subtrees absent on dest → dest prefix (copy-all, no HEADs).
    absent: Vec<String>,
}

/// PURE: classify one descended level. `src_cps`/`dest_cps` are child
/// common-prefixes (already dest-namespace for dest, source-namespace for src);
/// `rewrite` maps a source child prefix into the dest namespace.
fn step_level(
    dest_objects: &[(String, FileMetadata)],
    dest_cps: &[String],
    src_cps: &[String],
    rewrite: impl Fn(&str) -> Option<String>,
) -> LevelOutcome {
    let present: Vec<(String, DestLeaf)> = dest_objects
        .iter()
        .map(|(k, m)| {
            (
                k.clone(),
                DestLeaf {
                    file_size: m.file_size,
                    etag: m.md5.clone(),
                    created_at: m.created_at.timestamp_millis(),
                },
            )
        })
        .collect();
    let dest_set: std::collections::HashSet<&str> = dest_cps.iter().map(|s| s.as_str()).collect();
    // Dest child subtrees that exist → descend (compare finer).
    let descend: Vec<String> = dest_cps.to_vec();
    // Source child subtrees with no matching dest common-prefix → absent.
    let mut absent = Vec::new();
    for scp in src_cps {
        if let Some(dcp) = rewrite(scp) {
            if !dest_set.contains(dcp.as_str()) {
                absent.push(dcp);
            }
        }
    }
    LevelOutcome {
        present,
        descend,
        absent,
    }
}

/// Translate a SOURCE prefix into the destination namespace (the forward of
/// [`dest_to_source_key`]) — used to compare source children against dest
/// common-prefixes during the tree descent.
fn source_prefix_to_dest(rule: &ReplicationRule, src_prefix: &str) -> Option<String> {
    let dst = normalize_prefix(&rule.destination.prefix);
    let src = normalize_prefix(&rule.source.prefix);
    let (dst, src) = (dst.as_str(), src.as_str());
    if (dst.is_empty() && src.is_empty()) || dst == src {
        return Some(src_prefix.to_string());
    }
    if src.is_empty() {
        return Some(format!("{}{}", dst, src_prefix.trim_start_matches('/')));
    }
    let tail = src_prefix.strip_prefix(src)?;
    Some(format!("{}{}", dst, tail.trim_start_matches('/')))
}

/// Build the prefix-tree destination oracle by descending BOTH trees with a `/`
/// delimiter. Returns `Unbounded` (HEAD-every-key) on any list error, cap breach,
/// or kill/pause — never a partial `Known`. `dest_prefix`/`source_prefix` are the
/// normalized rule prefixes.
async fn build_dest_oracle(
    engine: &DynEngine,
    ctrl: &RunControl,
    rule: &ReplicationRule,
    dest_prefix: &str,
    source_prefix: &str,
) -> DestOracle {
    let mut present: std::collections::HashMap<String, DestLeaf> = std::collections::HashMap::new();
    let mut absent_subtrees: Vec<String> = Vec::new();
    // Frontier holds (dest_prefix, source_prefix) pairs to descend in lockstep.
    let mut frontier: std::collections::VecDeque<(String, String)> =
        std::collections::VecDeque::new();
    frontier.push_back((dest_prefix.to_string(), source_prefix.to_string()));
    let mut levels = 0usize;

    while let Some((dpfx, spfx)) = frontier.pop_front() {
        levels += 1;
        if levels > MAX_ORACLE_LEVELS
            || frontier.len() > MAX_ORACLE_FRONTIER
            || absent_subtrees.len() > MAX_ABSENT_SUBTREES
        {
            return DestOracle::Unbounded;
        }
        // Honor kill/pause/lease-loss during the (potentially long) descent —
        // the same uniform check as the copy loop (one-off ignores pause).
        // Bail to Unbounded: a partial Known over-marks absent → over-copy.
        match ctrl.check(false).await {
            Ok(ControlVerdict::Continue) => {}
            _ => return DestOracle::Unbounded,
        }

        // List the dest level (delimiter='/', metadata=true so leaves carry lite
        // facts for the HEAD-free policy compare), draining the level's own
        // pagination. Accumulate child common-prefixes across pages.
        let mut dest_cps: Vec<String> = Vec::new();
        let mut dest_objects: Vec<(String, FileMetadata)> = Vec::new();
        let mut pager = Pager::fresh();
        while pager.begin_page().is_some() {
            let page = match engine
                .list_objects(
                    &rule.destination.bucket,
                    &dpfx,
                    Some("/"),
                    LEVEL_PAGE_KEYS,
                    pager.token(),
                    true,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!("replication dest oracle list {}/{dpfx} failed ({e}); HEAD-every-key fallback", rule.destination.bucket);
                    return DestOracle::Unbounded;
                }
            };
            dest_cps.extend(page.common_prefixes.iter().cloned());
            dest_objects.extend(page.objects.iter().cloned());
            if pager.truncated_by_page_budget() {
                return DestOracle::Unbounded;
            }
            if !pager.advance(page.is_truncated, page.next_continuation_token.clone()) {
                break;
            }
        }
        // Post-loop guard: begin_page() returns None at the budget boundary and
        // exits the loop WITHOUT re-checking, so a level truncated exactly at the
        // last allowed page would otherwise be treated as complete → over-copy.
        if pager.truncated_by_page_budget() {
            return DestOracle::Unbounded;
        }

        // List the source level for child common-prefixes (lite — we only need
        // the prefix names to decide descend-vs-absent).
        let mut src_cps: Vec<String> = Vec::new();
        let mut spager = Pager::fresh();
        while spager.begin_page().is_some() {
            let page = match engine
                .list_objects(
                    &rule.source.bucket,
                    &spfx,
                    Some("/"),
                    LEVEL_PAGE_KEYS,
                    spager.token(),
                    false,
                )
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!("replication source oracle list {}/{spfx} failed ({e}); HEAD-every-key fallback", rule.source.bucket);
                    return DestOracle::Unbounded;
                }
            };
            src_cps.extend(page.common_prefixes.iter().cloned());
            if spager.truncated_by_page_budget() {
                return DestOracle::Unbounded;
            }
            if !spager.advance(page.is_truncated, page.next_continuation_token.clone()) {
                break;
            }
        }
        if spager.truncated_by_page_budget() {
            return DestOracle::Unbounded;
        }

        let outcome = step_level(&dest_objects, &dest_cps, &src_cps, |scp| {
            source_prefix_to_dest(rule, scp)
        });
        for (k, leaf) in outcome.present {
            present.insert(k, leaf);
            if present.len() > DEST_INDEX_MAX_KEYS {
                return DestOracle::Unbounded;
            }
        }
        absent_subtrees.extend(outcome.absent);
        // Descend shared subtrees: pair each dest child with its source prefix.
        for dcp in outcome.descend {
            if let Some(scp) = dest_to_source_key(rule, &dcp) {
                frontier.push_back((dcp, scp));
            }
        }
    }

    DestOracle::Known {
        present,
        absent_subtrees,
    }
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
    fn throttle_abort_fires_for_small_batch_full_page() {
        // H16: small batch_size (≤2) — a fully-throttling page must abort even
        // though throttled can never reach 3.
        assert!(page_is_throttle_aborted(0, 2, 2)); // whole 2-object page throttled
        assert!(page_is_throttle_aborted(0, 1, 1)); // whole 1-object page throttled
                                                    // A large page needs ≥3 (a stray 503 or two must NOT abort).
        assert!(!page_is_throttle_aborted(0, 2, 1000));
        assert!(page_is_throttle_aborted(0, 3, 1000));
        // Any success this page → never abort (backend is coping).
        assert!(!page_is_throttle_aborted(5, 100, 1000));
        // No throttles → never abort.
        assert!(!page_is_throttle_aborted(0, 0, 1000));
    }

    #[test]
    fn absent_prefix_matches_only_path_descendants() {
        // A live sibling must NEVER match a proven-absent subtree.
        assert!(key_under_absent_prefix("foo/x", "foo/"));
        assert!(key_under_absent_prefix("foo/bar/x", "foo/"));
        assert!(
            !key_under_absent_prefix("foobar/x", "foo/"),
            "sibling must not match"
        );
        assert!(!key_under_absent_prefix("food", "foo/"));
        // The prefix key itself (no trailing slash) is not "under" the subtree.
        assert!(!key_under_absent_prefix("foo", "foo/"));
        // Whole-bucket absent (empty prefix) matches everything.
        assert!(key_under_absent_prefix("anything/at/all", ""));
        // Defensive: a non-slash prefix (shouldn't occur) never matches.
        assert!(!key_under_absent_prefix("foobar/x", "foo"));
        assert!(!key_under_absent_prefix("foo/x", "foo"));
    }

    #[test]
    fn control_verdict_truth_table() {
        use ControlVerdict::*;
        // (cancel, paused, one_off, lease_ok) -> verdict
        let cases = [
            ((false, false, false, true), Continue),
            ((false, false, true, true), Continue),
            // Paused stops a scheduled run but NOT a one-off (run-now contract).
            ((false, true, false, true), Paused),
            ((false, true, true, true), Continue),
            // Kill outranks everything, one-off included.
            ((true, false, false, true), Killed),
            ((true, true, true, true), Killed),
            ((true, false, false, false), Killed),
            // Lease loss outranks pause, applies to one-offs too.
            ((false, true, false, false), LeaseLost),
            ((false, false, true, false), LeaseLost),
            ((false, true, true, false), LeaseLost),
        ];
        for ((cancel, paused, one_off, lease_ok), want) in cases {
            assert_eq!(
                control_verdict(cancel, paused, one_off, lease_ok),
                want,
                "cancel={cancel} paused={paused} one_off={one_off} lease_ok={lease_ok}"
            );
        }
    }

    #[test]
    fn maintenance_overlay_defers_only_from_continue() {
        use ControlVerdict::*;
        // maint_busy turns a Continue into Paused (mid-run defer) — even for a
        // one-off (must not write into a bucket being rewritten).
        assert_eq!(
            control_verdict_with_maintenance(false, false, false, true, true),
            Paused
        );
        assert_eq!(
            control_verdict_with_maintenance(false, false, true, true, true),
            Paused,
            "one-off is NOT exempt from the maintenance defer"
        );
        // Without maint_busy, base verdict is unchanged.
        assert_eq!(
            control_verdict_with_maintenance(false, false, false, true, false),
            Continue
        );
        // Higher-precedence verdicts are NOT masked by the maintenance overlay.
        assert_eq!(
            control_verdict_with_maintenance(true, false, false, true, true),
            Killed,
            "kill must win over a maintenance defer"
        );
        assert_eq!(
            control_verdict_with_maintenance(false, false, false, false, true),
            LeaseLost,
            "lease loss must win over a maintenance defer"
        );
        // Already-Paused stays Paused (idempotent overlay).
        assert_eq!(
            control_verdict_with_maintenance(false, true, false, true, true),
            Paused
        );
    }

    #[test]
    fn settle_run_truth_table() {
        let base = SettleInput {
            killed: false,
            stopped_paused: false,
            hit_fatal_error: false,
            had_any_error: false,
            objects_copied: 0,
            truncated: false,
        };
        let d = settle_run(base);
        assert_eq!((d.status, d.clear_cursor), ("succeeded", true));

        // Budget truncation: NEVER clear the cursor — the tail must resume.
        let d = settle_run(SettleInput {
            truncated: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("stopped", false));

        let d = settle_run(SettleInput {
            killed: true,
            truncated: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("cancelled", false));

        let d = settle_run(SettleInput {
            stopped_paused: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("stopped", false));

        let d = settle_run(SettleInput {
            hit_fatal_error: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("failed", false));

        // Errors with zero progress = failed; with progress = partial.
        let d = settle_run(SettleInput {
            had_any_error: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("failed", true));
        let d = settle_run(SettleInput {
            had_any_error: true,
            objects_copied: 5,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("completed_with_errors", true));

        // Truncated + nonfatal errors: still resumable, cursor kept.
        let d = settle_run(SettleInput {
            had_any_error: true,
            objects_copied: 5,
            truncated: true,
            ..base
        });
        assert_eq!((d.status, d.clear_cursor), ("stopped", false));
    }

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
            // Real Backblaze B2 over-cap + dead-creds shapes (C2).
            "S3 error: put_object failed (status=403): cap_exceeded",
            "Cannot upload, would exceed account cap",
            "all access to this object has been disabled",
            "S3 error: AccessDenied",
            "SignatureDoesNotMatch: the request signature we calculated",
            "PermanentRedirect: the bucket is in a different region",
        ] {
            assert!(is_destination_fatal(f), "expected fatal: {f}");
        }
        // Per-object hiccups: NOT fatal — keep going. (Note: tokens like a bare
        // "quota" or "access denied" CAN be per-object, but the fold's
        // page-level zero-success gate stops one such object aborting a healthy
        // page — this fn only answers "could this be a dead dest".)
        for ok in [
            "object copy timed out after 1800s",
            "S3 error: put_object failed (status=503): SlowDown",
            "connection reset by peer",
            "NoSuchKey",
        ] {
            assert!(!is_destination_fatal(ok), "expected non-fatal: {ok}");
        }
    }

    #[test]
    fn backend_throttled_truth_table() {
        // Throttle signals: Ceph/AWS SlowDown, StorageError::Throttled
        // Display, generic 429, bare 503 statuses.
        for t in [
            "S3 error: put_object failed (status=503): SlowDown",
            "Backend throttled: get_object throttled (status=503): service error",
            "429 Too Many Requests",
            "head_object failed (status=503)",
            "service error (503 Service Unavailable)",
        ] {
            assert!(is_backend_throttled(t), "expected throttled: {t}");
        }
        // Everything else: not a throttle — per-object handling continues.
        for ok in [
            "Bucket not found: beshu-b2",
            "object copy timed out after 1800s",
            "connection reset by peer",
            "NoSuchKey",
            "S3 error: AccessDenied",
            "storage error: quota exceeded",
        ] {
            assert!(!is_backend_throttled(ok), "expected non-throttle: {ok}");
        }
    }

    fn leaf(size: u64) -> DestLeaf {
        DestLeaf {
            file_size: size,
            etag: "etag".into(),
            created_at: 0,
        }
    }

    #[test]
    fn oracle_may_contain_truth_table() {
        // Unbounded → everything may be present (HEAD-every-key fallback).
        let u = DestOracle::Unbounded;
        assert!(u.may_contain("anything"));
        assert!(u.leaf("anything").is_none());

        let mut present = std::collections::HashMap::new();
        present.insert("builds/a.txt".to_string(), leaf(10));
        let k = DestOracle::Known {
            present,
            absent_subtrees: vec!["mirror/".to_string()],
        };
        // Present key → may contain + leaf available.
        assert!(k.may_contain("builds/a.txt"));
        assert_eq!(k.leaf("builds/a.txt").map(|l| l.file_size), Some(10));
        // Key under a proven-absent subtree → definitely missing (copy, no HEAD).
        assert!(!k.may_contain("mirror/anything/deep.bin"));
        // Key neither present nor under an absent subtree → missing (no leaf).
        assert!(!k.may_contain("builds/b.txt"));
        assert!(k.leaf("builds/b.txt").is_none());
    }

    #[test]
    fn step_level_classifies_descend_absent_present() {
        // Dest has child "shared/" and a leaf "f.txt"; source has children
        // "shared/" and "fresh/". → descend shared/, mark fresh/ absent, present f.txt.
        let dest_objs = vec![(
            "f.txt".to_string(),
            FileMetadata::fallback(
                "f.txt".into(),
                5,
                "e".into(),
                chrono::Utc::now(),
                None,
                crate::types::StorageInfo::Passthrough,
            ),
        )];
        let dest_cps = vec!["shared/".to_string()];
        let src_cps = vec!["shared/".to_string(), "fresh/".to_string()];
        let out = step_level(&dest_objs, &dest_cps, &src_cps, |p| Some(p.to_string()));
        assert_eq!(out.descend, vec!["shared/".to_string()]);
        assert_eq!(out.absent, vec!["fresh/".to_string()]);
        assert_eq!(out.present.len(), 1);
        assert_eq!(out.present[0].0, "f.txt");
        assert_eq!(out.present[0].1.file_size, 5);
    }

    #[test]
    fn step_level_empty_dest_marks_all_source_absent() {
        let src_cps = vec!["a/".to_string(), "b/".to_string()];
        let out = step_level(&[], &[], &src_cps, |p| Some(p.to_string()));
        assert!(out.descend.is_empty());
        assert!(out.present.is_empty());
        assert_eq!(out.absent, vec!["a/".to_string(), "b/".to_string()]);
    }

    #[test]
    fn source_prefix_to_dest_rewrites() {
        let mut rule = mk_rule();
        // identity when both empty
        assert_eq!(
            source_prefix_to_dest(&rule, "builds/"),
            Some("builds/".to_string())
        );
        rule.source.prefix = "builds/".into();
        rule.destination.prefix = "mirror/".into();
        assert_eq!(
            source_prefix_to_dest(&rule, "builds/releases/"),
            Some("mirror/releases/".to_string())
        );
        // a source child outside the source prefix → None (not this rule's).
        assert_eq!(source_prefix_to_dest(&rule, "other/"), None);
    }

    #[test]
    fn dest_leaf_synth_meta_round_trips_size_etag() {
        let l = DestLeaf {
            file_size: 42,
            etag: "abc".into(),
            created_at: 1_700_000_000_000,
        };
        let m = l.synth_meta("builds/x.bin");
        assert_eq!(m.file_size, 42);
        assert_eq!(m.md5, "abc");
        assert!(m.file_sha256.is_empty()); // lite never carries sha
        assert_eq!(m.created_at.timestamp_millis(), 1_700_000_000_000);
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
            strict_content_diff: false,
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
