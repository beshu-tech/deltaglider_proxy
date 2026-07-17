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

use super::planner::{compile_rule_globs, normalize_prefix};
use super::state_store::{current_unix_seconds, FailureInsert, RunTotals};
use super::walk;
use crate::background::RunLease;
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationRule;
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::job_loop::MAX_JOB_PAGES;
use crate::metrics::{bump_peak, Metrics};
use crate::transfer::{
    copy_object_with_retries, CopyStrategy, ObjectTransferRequest, TransferProvenance,
    REPLICATION_RULE_METADATA_KEY,
};
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

/// Live walk progress for the Jobs UI (dirs completed / dirs pending),
/// updated at each driver checkpoint. In-memory and node-local by design —
/// live progress, not durable state.
static WALK_PROGRESS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, (u64, u64)>>,
> = std::sync::LazyLock::new(Default::default);

/// Snapshot of a rule's walk progress for the admin jobs API.
pub fn walk_snapshot(rule: &str) -> Option<(u64, u64)> {
    WALK_PROGRESS.lock().get(rule).copied()
}

/// RAII: clears the walk-progress entry when the run ends, however it ends.
struct WalkProgressGuard(String);

impl Drop for WalkProgressGuard {
    fn drop(&mut self) {
        WALK_PROGRESS.lock().remove(&self.0);
    }
}

/// One completed driver operation, fed back into the WalkMachine (plus the
/// driver-side bookkeeping the machine doesn't need: totals, abort flags).
enum DriverDone {
    List {
        req_id: u64,
        result: Result<walk::RelListing, String>,
    },
    Heads {
        req_id: u64,
        side: walk::Side,
        results: Vec<(String, walk::HeadResult)>,
    },
    Copy {
        item_id: u64,
        res: Result<Box<PerObjectResult>, crate::config_db::ConfigDbError>,
    },
    Delete {
        item_id: u64,
        deleted: bool,
        error: bool,
    },
}

/// (bucket, normalized rule prefix) for one side of the walk.
fn side_target(
    rule: &ReplicationRule,
    source_prefix: &str,
    dest_prefix: &str,
    side: walk::Side,
) -> (String, String) {
    match side {
        walk::Side::Src => (rule.source.bucket.clone(), source_prefix.to_string()),
        walk::Side::Dest => (rule.destination.bucket.clone(), dest_prefix.to_string()),
    }
}

/// Convert an engine listing page into the machine's relative form: strip the
/// side prefix VERBATIM (never normalize — `a//x` stays distinct from `a/x`)
/// and interleave objects + collapsed prefixes in engine (lexicographic)
/// order. Engine continuation tokens are user-visible keys under the listing
/// prefix on every backend, so they strip the same way; a key outside the
/// prefix is an engine bug and fails the listing loudly (safe direction).
fn to_rel_listing(
    page: crate::deltaglider::ListObjectsPage,
    side_prefix: &str,
) -> Result<walk::RelListing, String> {
    let strip = |k: &str| -> Result<String, String> {
        k.strip_prefix(side_prefix)
            .map(str::to_string)
            .ok_or_else(|| format!("listing entry {k:?} outside prefix {side_prefix:?}"))
    };
    let mut entries: Vec<walk::RelEntry> =
        Vec::with_capacity(page.objects.len() + page.common_prefixes.len());
    let mut objs = page.objects.into_iter().peekable();
    let mut dirs = page.common_prefixes.into_iter().peekable();
    loop {
        let take_obj = match (objs.peek(), dirs.peek()) {
            (Some((ok, _)), Some(dp)) => ok.as_str() <= dp.as_str(),
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_obj {
            let (k, meta) = objs.next().expect("peeked");
            entries.push(walk::RelEntry {
                path: strip(&k)?,
                kind: walk::EntryKind::Obj(Box::new(meta)),
            });
        } else {
            let d = dirs.next().expect("peeked");
            entries.push(walk::RelEntry {
                path: strip(&d)?,
                kind: walk::EntryKind::Dir,
            });
        }
    }
    let next_token = match page.next_continuation_token {
        Some(t) => Some(strip(&t)?),
        None => None,
    };
    Ok(walk::RelListing {
        entries,
        truncated: page.is_truncated,
        next_token,
    })
}

/// The delete pipeline, verbatim old `run_delete_pass` per-key semantics:
/// provenance HEAD-confirm when the listing lacked our marker (HEAD error →
/// preserve), source-absence HEAD (delete ONLY on NoSuchKey; any other error
/// → preserve + failure ring), then the destination delete.
#[allow(clippy::too_many_arguments)]
async fn execute_delete(
    db: &Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule_name: &str,
    src_bucket: &str,
    dst_bucket: &str,
    abs_src: &str,
    abs_dest: &str,
    item_id: u64,
    needs_provenance_head: bool,
    run_id: i64,
    max_failures_retained: u32,
) -> DriverDone {
    if needs_provenance_head {
        if let Some(m) = engine.metrics() {
            m.replication_head_calls_total.inc();
        }
        let owned = match engine.head(dst_bucket, abs_dest).await {
            Ok(meta) => super::event_consumer::owned_by_rule(&meta, rule_name),
            // HEAD failed — preserve. Better a leftover copy than a
            // false-delete of a foreign object.
            Err(_) => false,
        };
        if !owned {
            return DriverDone::Delete {
                item_id,
                deleted: false,
                error: false,
            };
        }
    }
    if let Some(m) = engine.metrics() {
        m.replication_head_calls_total.inc();
    }
    match engine.head(src_bucket, abs_src).await {
        Ok(_) => DriverDone::Delete {
            item_id,
            deleted: false,
            error: false, // source reappeared — nothing to replicate
        },
        Err(e) => {
            let s3e: crate::api::S3Error = e.into();
            if matches!(s3e, crate::api::S3Error::NoSuchKey(_)) {
                // Same test-only stall as the copy path: lets a kill land
                // deterministically mid-delete (inert in prod).
                maybe_pass_stall().await;
                match engine.delete(dst_bucket, abs_dest).await {
                    Ok(_) => DriverDone::Delete {
                        item_id,
                        deleted: true,
                        error: false,
                    },
                    Err(de) => {
                        if let Err(le) = log_failure(
                            db,
                            rule_name,
                            run_id,
                            abs_src,
                            abs_dest,
                            &format!("destination delete failed: {de}"),
                            max_failures_retained,
                        )
                        .await
                        {
                            warn!(
                                "replication rule '{rule_name}': failure-ring write failed: {le}"
                            );
                        }
                        DriverDone::Delete {
                            item_id,
                            deleted: false,
                            error: true,
                        }
                    }
                }
            } else {
                if let Err(le) = log_failure(
                    db,
                    rule_name,
                    run_id,
                    abs_src,
                    abs_dest,
                    &format!("delete-pass head source failed: {s3e}"),
                    max_failures_retained,
                )
                .await
                {
                    warn!("replication rule '{rule_name}': failure-ring write failed: {le}");
                }
                DriverDone::Delete {
                    item_id,
                    deleted: false,
                    error: true,
                }
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
/// per page; `upload_concurrency` = in-flight parts per streaming object;
/// `dir_concurrency` = concurrent directory listings in the reconcile walk.
#[derive(Debug, Clone, Copy)]
pub struct RunConcurrency {
    pub transfers: u32,
    pub upload_concurrency: u32,
    pub dir_concurrency: u32,
}

impl Default for RunConcurrency {
    fn default() -> Self {
        Self {
            transfers: crate::transfer_plan::TRANSFERS as u32,
            upload_concurrency: crate::transfer_plan::UPLOAD_CONCURRENCY as u32,
            dir_concurrency: 4,
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

    // ── Fused directory-scoped tree walk ──
    //
    // The pure WalkMachine (`replication::walk`) owns every decision: per-dir
    // src/dest merge, policy/regime-driven HEAD resolution (PureMirror ⇒ zero
    // HEADs), per-dir provenance-gated deletes, the resume watermark, the page
    // budget, and the flat-sweep degrade. This driver only executes commands
    // against the engine and feeds results back — copies start with the first
    // directory, no discovery pre-pass.
    let scope = walk::cursor_scope(
        &rule.source.bucket,
        &source_prefix,
        &rule.destination.bucket,
        &dest_prefix,
    );
    let resume = walk::load_cursor(continuation.as_deref(), &scope);
    let was_resumed = resume.is_some();
    if continuation.is_some() && !was_resumed {
        info!(
            "replication rule '{}': stale/legacy cursor discarded — starting a fresh walk",
            rule.name
        );
    }
    let regime = walk::FactRegime {
        src_lite_authoritative: engine.lite_list_carries_logical_facts(&rule.source.bucket),
        dest_lite_authoritative: engine.lite_list_carries_logical_facts(&rule.destination.bucket),
    };
    let max_pages = match crate::config::env_parse::<u32>("DGP_TEST_MAX_JOB_PAGES") {
        Some(n) if n > 0 => n,
        _ => MAX_JOB_PAGES,
    };
    let dir_workers = concurrency.dir_concurrency.clamp(1, 16) as usize;
    let mut machine = match compile_rule_globs(rule) {
        Ok((include_globs, exclude_globs)) => Some(walk::WalkMachine::new(
            walk::WalkConfig {
                scope,
                rule_name: rule.name.clone(),
                src_prefix: source_prefix.clone(),
                dest_prefix: dest_prefix.clone(),
                conflict: rule.conflict,
                strict_content_diff: rule.strict_content_diff,
                replicate_deletes: rule.replicate_deletes,
                include_globs,
                exclude_globs,
                regime,
                page_size: cap,
                max_pages,
                dir_workers,
                head_batch: 100,
                max_child_dirs_per_dir: 10_000,
                max_pending_dirs: 500_000,
                action_buffer: (transfers * 8).max(64),
            },
            resume,
        )),
        Err(e) => {
            warn!("replication rule '{}' glob compile failed: {e}", rule.name);
            totals.errors += 1;
            hit_fatal_error = true;
            None
        }
    };

    // Poison-skips recorded by copy_one_object (folded from PerObjectResult);
    // planner skips come from the machine's stats. Both feed objects_skipped.
    let mut poison_skipped: i64 = 0;
    let _progress_guard = WalkProgressGuard(rule.name.clone());

    if let Some(machine) = machine.as_mut() {
        let mut inflight: futures::stream::FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = DriverDone> + Send>>,
        > = futures::stream::FuturesUnordered::new();
        let (mut lists_inflight, mut heads_inflight) = (0usize, 0usize);
        let (mut copies_inflight, mut deletes_inflight) = (0usize, 0usize);
        // Rolling abort-classification window, reset at each control boundary
        // (the old page boundary): dest-fatal and throttle-abort gates keep
        // their zero-successes semantics over the window.
        let (mut win_attempted, mut win_copied, mut win_throttled) = (0i64, 0i64, 0i64);
        let mut clear_failure_keys: Vec<String> = Vec::new();
        let mut last_persisted_pos = machine.cursor().map(|c| c.pos);
        let mut last_dirs_reported: u64 = 0;
        let mut first_list_done = false;
        let mut events_since_flush = 0usize;
        let mut need_check = true;
        let mut kill_watch = Box::pin(poll_run_killed(&db, run_id));

        'walk: loop {
            // Control boundary (kill / pause / lease renew / maintenance),
            // page-boundary cadence: every completed listing re-arms it.
            if need_check {
                need_check = false;
                win_attempted = 0;
                win_copied = 0;
                win_throttled = 0;
                match ctrl.check(true).await {
                    Err(e) => {
                        warn!(
                            "replication rule '{}': control check failed: {e}",
                            rule.name
                        );
                        totals.errors += 1;
                        hit_fatal_error = true;
                        machine.drain(walk::DrainReason::Fatal);
                        break 'walk;
                    }
                    Ok(ControlVerdict::Continue) => {}
                    Ok(ControlVerdict::Killed) => {
                        info!("replication rule '{}' killed mid-walk", rule.name);
                        killed = true;
                        machine.drain(walk::DrainReason::Killed);
                        break 'walk;
                    }
                    Ok(ControlVerdict::Paused) => {
                        info!(
                            "replication rule '{}' paused mid-walk (cursor preserved for resume)",
                            rule.name
                        );
                        stopped_paused = true;
                        machine.drain(walk::DrainReason::Paused);
                        break 'walk;
                    }
                    Ok(ControlVerdict::LeaseLost) => {
                        totals.errors += 1;
                        hit_fatal_error = true;
                        machine.drain(walk::DrainReason::LeaseLost);
                        break 'walk;
                    }
                }
            }

            let caps = walk::DriverCaps {
                list: (dir_workers * 2).saturating_sub(lists_inflight),
                head: 2usize.saturating_sub(heads_inflight),
                copy: transfers.saturating_sub(copies_inflight),
                delete: 2usize.saturating_sub(deletes_inflight),
            };
            let cmds = machine.poll(caps);
            let dispatched = !cmds.is_empty();
            for cmd in cmds {
                match cmd {
                    walk::Cmd::List {
                        req_id,
                        side,
                        rel_prefix,
                        token,
                        delimited,
                    } => {
                        lists_inflight += 1;
                        let engine = engine.clone();
                        let (bucket, side_prefix) =
                            side_target(rule, &source_prefix, &dest_prefix, side);
                        let page_size = cap;
                        inflight.push(Box::pin(async move {
                            let abs_prefix = format!("{side_prefix}{rel_prefix}");
                            let abs_token = token.map(|t| format!("{side_prefix}{t}"));
                            if let Some(m) = engine.metrics() {
                                m.replication_list_calls_total.inc();
                            }
                            let result = engine
                                .list_objects(
                                    &bucket,
                                    &abs_prefix,
                                    delimited.then_some("/"),
                                    page_size,
                                    abs_token.as_deref(),
                                    false,
                                )
                                .await
                                .map_err(|e| e.to_string())
                                .and_then(|p| to_rel_listing(p, &side_prefix));
                            DriverDone::List { req_id, result }
                        }));
                    }
                    walk::Cmd::Head {
                        req_id,
                        side,
                        rel_keys,
                    } => {
                        heads_inflight += 1;
                        let engine = engine.clone();
                        let (bucket, side_prefix) =
                            side_target(rule, &source_prefix, &dest_prefix, side);
                        inflight.push(Box::pin(async move {
                            let mut results = Vec::with_capacity(rel_keys.len());
                            for rel in rel_keys {
                                let abs = format!("{side_prefix}{rel}");
                                if let Some(m) = engine.metrics() {
                                    m.replication_head_calls_total.inc();
                                }
                                let r = match engine.head(&bucket, &abs).await {
                                    Ok(meta) => walk::HeadResult::Resolved(Box::new(meta)),
                                    Err(e) => {
                                        let s3e: crate::api::S3Error = e.into();
                                        if matches!(s3e, crate::api::S3Error::NoSuchKey(_)) {
                                            walk::HeadResult::Gone
                                        } else {
                                            walk::HeadResult::Unresolved
                                        }
                                    }
                                };
                                results.push((rel, r));
                            }
                            DriverDone::Heads {
                                req_id,
                                side,
                                results,
                            }
                        }));
                    }
                    walk::Cmd::Copy {
                        item_id,
                        rel_key,
                        src_size,
                    } => {
                        copies_inflight += 1;
                        let db = db.clone();
                        let engine = engine.clone();
                        let rule_name = rule.name.clone();
                        let src_bucket = rule.source.bucket.clone();
                        let dst_bucket = rule.destination.bucket.clone();
                        let events = events_sink.clone();
                        let abs_src = format!("{source_prefix}{rel_key}");
                        let abs_dest = format!("{dest_prefix}{rel_key}");
                        inflight.push(Box::pin(async move {
                            // Guard increments objects_inflight (+peak) on entry and
                            // decrements on drop → proves the `transfers` concurrency.
                            let _obj_guard = engine.metrics().cloned().map(ObjectGuard::new);
                            // Live "currently copying" registration for the Jobs UI
                            // (RAII: a kill dropping this future unregisters it).
                            let _inflight_reg = InFlightGuard::new(&rule_name, &abs_src, src_size);
                            let res = copy_one_object(
                                &db,
                                &engine,
                                &rule_name,
                                &src_bucket,
                                &dst_bucket,
                                &abs_src,
                                &abs_dest,
                                run_id,
                                object_timeout,
                                object_skip_after_failures,
                                upload_concurrency,
                                max_failures_retained,
                                &events,
                            )
                            .await;
                            DriverDone::Copy {
                                item_id,
                                res: res.map(Box::new),
                            }
                        }));
                    }
                    walk::Cmd::Delete {
                        item_id,
                        rel_key,
                        needs_provenance_head,
                    } => {
                        deletes_inflight += 1;
                        let db = db.clone();
                        let engine = engine.clone();
                        let rule_name = rule.name.clone();
                        let src_bucket = rule.source.bucket.clone();
                        let dst_bucket = rule.destination.bucket.clone();
                        let abs_src = format!("{source_prefix}{rel_key}");
                        let abs_dest = format!("{dest_prefix}{rel_key}");
                        inflight.push(Box::pin(async move {
                            execute_delete(
                                &db,
                                &engine,
                                &rule_name,
                                &src_bucket,
                                &dst_bucket,
                                &abs_src,
                                &abs_dest,
                                item_id,
                                needs_provenance_head,
                                run_id,
                                max_failures_retained,
                            )
                            .await
                        }));
                    }
                }
            }

            if inflight.is_empty() {
                if !dispatched {
                    break 'walk; // done, truncated, drained, or failed
                }
                continue;
            }

            // Await one completion, racing the operator kill so a wedged
            // object aborts NOW (dropping `inflight` cancels every transfer).
            let done = tokio::select! {
                biased;
                killed_now = &mut kill_watch => {
                    if killed_now {
                        killed = true;
                    }
                    machine.drain(walk::DrainReason::Killed);
                    break 'walk;
                }
                done = inflight.next() => match done {
                    Some(d) => d,
                    None => break 'walk,
                },
            };

            match done {
                DriverDone::List { req_id, result } => {
                    lists_inflight -= 1;
                    need_check = true;
                    match result {
                        Ok(page) => {
                            first_list_done = true;
                            machine.on_event(walk::Event::ListPage { req_id, page });
                        }
                        Err(msg) => {
                            warn!("replication rule '{}' list failed: {}", rule.name, msg);
                            // Poison-token guard: a RESUMED walk whose FIRST
                            // listing fails most likely holds a stale cursor —
                            // clear it so the next tick starts fresh.
                            if was_resumed && !first_list_done {
                                let db = db.lock().await;
                                let _ = db.replication_set_continuation_token(&rule.name, None);
                            }
                            if let Err(le) = log_failure(
                                &db,
                                &rule.name,
                                run_id,
                                "",
                                "",
                                &format!("list failed: {msg}"),
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
                            machine.on_event(walk::Event::ListFailed { req_id });
                        }
                    }
                }
                DriverDone::Heads {
                    req_id,
                    side,
                    results,
                } => {
                    heads_inflight -= 1;
                    machine.on_event(walk::Event::HeadDone { req_id, results });
                    let _ = side;
                }
                DriverDone::Copy { item_id, res } => {
                    copies_inflight -= 1;
                    match res {
                        Err(e) => {
                            warn!(
                                "replication rule '{}': per-object DB write failed: {e}",
                                rule.name
                            );
                            totals.errors += 1;
                            hit_fatal_error = true;
                            machine.drain(walk::DrainReason::Fatal);
                            machine.on_event(walk::Event::CopySettled { item_id, ok: false });
                        }
                        Ok(r) => {
                            if let Some(k) = r.clear_failure_key.clone() {
                                clear_failure_keys.push(k);
                            }
                            totals.objects_copied += r.objects_copied;
                            poison_skipped += r.objects_skipped;
                            totals.bytes_copied += r.bytes_copied;
                            totals.errors += r.errors;
                            totals.delta_passthrough += r.delta_passthrough;
                            totals.bytes_egress_saved += r.bytes_egress_saved;
                            win_attempted += 1;
                            win_copied += r.objects_copied;
                            if r.throttled {
                                win_throttled += 1;
                            }
                            if r.had_error {
                                had_any_error = true;
                            }
                            machine.on_event(walk::Event::CopySettled {
                                item_id,
                                ok: !r.had_error,
                            });
                            // Destination unusable (bucket missing / over quota):
                            // abort instead of retrying every remaining object.
                            // Gated on zero successes this window — a stray token
                            // in one error must not abort a healthy run.
                            if r.dest_fatal && win_copied == 0 {
                                warn!(
                                    "replication rule '{}' aborting run: destination unusable (bucket missing or over quota)",
                                    rule.name
                                );
                                hit_fatal_error = true;
                                dest_unusable = true;
                                machine.drain(walk::DrainReason::Fatal);
                            }
                            // Backend shedding load (503 SlowDown / 429): abort with
                            // backoff instead of grinding the key list.
                            if page_is_throttle_aborted(win_copied, win_throttled, win_attempted) {
                                warn!(
                                    "replication rule '{}' aborting run: backend throttled ({} SlowDown rejections this window)",
                                    rule.name, win_throttled
                                );
                                let _ = log_failure(
                                    &db,
                                    &rule.name,
                                    run_id,
                                    "",
                                    "",
                                    &format!(
                                        "run aborted: backend throttled ({win_throttled} SlowDown rejections); \
                                         resuming from cursor after backoff"
                                    ),
                                    max_failures_retained,
                                )
                                .await;
                                hit_fatal_error = true;
                                backend_throttled = true;
                                machine.drain(walk::DrainReason::Fatal);
                            }
                        }
                    }
                }
                DriverDone::Delete {
                    item_id,
                    deleted,
                    error,
                } => {
                    deletes_inflight -= 1;
                    if deleted {
                        totals.objects_deleted += 1;
                    }
                    if error {
                        totals.errors += 1;
                        had_any_error = true;
                    }
                    machine.on_event(walk::Event::DeleteSettled {
                        item_id,
                        ok: !error,
                    });
                }
            }

            // Fused checkpoint when the durable position moved (or every 32
            // events for progress/event visibility during long stretches):
            // failure-ledger clears + cursor + run progress + event flush
            // under ONE db.lock — the old per-page contract.
            events_since_flush += 1;
            if events_since_flush >= 64 {
                need_check = true;
            }
            let cur_pos = machine.cursor().map(|c| c.pos);
            if cur_pos != last_persisted_pos || events_since_flush >= 32 {
                let stats = machine.stats().clone();
                totals.objects_scanned = stats.objects_scanned as i64;
                totals.objects_skipped = stats.objects_skipped as i64 + poison_skipped;
                if let Some(m) = engine.metrics() {
                    let delta = stats.dirs_completed.saturating_sub(last_dirs_reported);
                    if delta > 0 {
                        m.replication_dirs_completed_total.inc_by(delta);
                        last_dirs_reported = stats.dirs_completed;
                    }
                }
                let (dirs_done, dirs_pending) = machine.progress();
                WALK_PROGRESS
                    .lock()
                    .insert(rule.name.clone(), (dirs_done, dirs_pending));
                let cursor_json = machine.cursor().map(|c| c.to_json());
                let db = db.lock().await;
                for k in clear_failure_keys.drain(..) {
                    let _ = db.replication_clear_object_failure(&rule.name, &k);
                }
                let persist = db
                    .replication_set_continuation_token(&rule.name, cursor_json.as_deref())
                    .and_then(|_| db.replication_update_run_progress(run_id, totals));
                let mut drained: Vec<NewEvent> = std::mem::take(&mut *events_sink.lock());
                flush_page_events_locked(&db, &rule.name, &mut drained);
                drop(db);
                if let Err(e) = persist {
                    warn!(
                        "replication rule '{}': cursor/progress persist failed: {e}",
                        rule.name
                    );
                    totals.errors += 1;
                    hit_fatal_error = true;
                    machine.drain(walk::DrainReason::Fatal);
                }
                last_persisted_pos = cur_pos;
                events_since_flush = 0;
            }
        }

        // Final stats sync (the loop may have broken between checkpoints).
        let stats = machine.stats().clone();
        totals.objects_scanned = stats.objects_scanned as i64;
        totals.objects_skipped = stats.objects_skipped as i64 + poison_skipped;
        if let Some(m) = engine.metrics() {
            let delta = stats.dirs_completed.saturating_sub(last_dirs_reported);
            if delta > 0 {
                m.replication_dirs_completed_total.inc_by(delta);
            }
        }
        // Any pending failure-ledger clears from the tail of the run.
        if !clear_failure_keys.is_empty() {
            let db = db.lock().await;
            for k in clear_failure_keys.drain(..) {
                let _ = db.replication_clear_object_failure(&rule.name, &k);
            }
        }
    }

    // Unconditional flush: covers EVERY break path (kill, pause, lease,
    // dest-fatal) — events pushed by durably-completed copies must survive.
    flush_event_sink(&db, &rule.name, &events_sink).await;
    // Walk ran out of page budget (or stopped short of completion for any
    // non-terminal reason): the cursor stays persisted so the next tick
    // resumes the tail (never reported as a clean pass).
    let truncated = machine.as_ref().is_some_and(|m| {
        m.truncated_by_budget() || (!killed && !stopped_paused && !hit_fatal_error && !m.is_done())
    });
    let final_cursor_json = machine
        .as_ref()
        .and_then(|m| m.cursor())
        .map(|c| c.to_json());

    // Deletes are fused into the walk: each directory's provenance-gated
    // candidates flush when THAT directory completes cleanly (per-dir gate,
    // strictly narrower blast radius than the old whole-run clean-pass gate).

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
        } else if let Some(json) = final_cursor_json.as_deref() {
            // Persist the FINAL watermark (checkpoints may lag it) so the
            // resumed run redoes as little as possible.
            db.replication_set_continuation_token(&rule.name, Some(json))?;
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
}
