// SPDX-License-Identifier: BUSL-1.1

//! Lifecycle execution through the DeltaGlider engine.

use super::planner::{
    compile_rule_globs, is_internal_key, lifecycle_prefix, plan_object, plan_retain_newest,
    Candidate, Decision, PlannedLifecycleAction, QualifySpec, SkipReason,
};
use super::state_store::{LifecycleFailureInsert, LifecycleRunTotals};
use crate::background::RunLease;
use crate::config_db::ConfigDb;
use crate::config_sections::{LifecycleAction, LifecycleRetainNewestAction, LifecycleRule};
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::job_loop::Pager;
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
    pub size: u64,
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
    pub candidates: Vec<PreviewObject>,
    pub failures: Vec<LifecycleFailure>,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

pub async fn preview_rule(
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_candidates: usize,
) -> Result<LifecycleRunOutcome, String> {
    // Preview never writes → no maintenance gate needed.
    run_or_preview(None, engine, rule, max_candidates, false, None, None).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_rule(
    db: Option<Arc<Mutex<ConfigDb>>>,
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
    db: Option<&Arc<Mutex<ConfigDb>>>,
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
    db: Option<Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_failures_retained: u32,
    run_id: Option<i64>,
    next_due_delay_secs: i64,
    lease: Option<RunLease>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> Result<LifecycleRunOutcome, String> {
    let lease_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat = RunLeaseGuard {
        heartbeat: spawn_lease_heartbeat(
            db.clone(),
            &rule.name,
            lease.clone(),
            lease_alive.clone(),
        ),
        release: db
            .clone()
            .zip(lease.as_ref())
            .map(|(db, l)| (db, rule.name.clone(), l.owner.clone())),
    };

    let ctx = RunContext {
        run_id,
        max_failures_retained,
        lease,
        lease_alive,
    };
    let outcome_result = run_or_preview(
        db.clone(),
        engine,
        rule,
        max_failures_retained as usize,
        true,
        Some(ctx.clone()),
        maintenance_gate.clone(),
    )
    .await;
    // Normal exit: stop renewing. The caller releases the lease it took.
    heartbeat.finish();

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
        let totals = LifecycleRunTotals {
            objects_scanned: outcome.objects_scanned,
            objects_affected: outcome.objects_affected,
            objects_skipped: outcome.objects_skipped,
            bytes_affected: outcome.bytes_affected,
            errors: outcome.errors,
        };
        let finished_at = super::current_unix_seconds();
        let db = db.lock().await;
        db.lifecycle_finish_run(
            run_id,
            &rule.name,
            &outcome.status,
            finished_at,
            totals,
            finished_at.saturating_add(next_due_delay_secs.max(1)),
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
    heartbeat: Option<tokio::task::JoinHandle<()>>,
    /// (db, rule, owner) to release on an abnormal exit.
    release: Option<(Arc<Mutex<ConfigDb>>, String, String)>,
}

impl RunLeaseGuard {
    /// Normal exit: the caller releases the lease itself.
    fn finish(mut self) {
        self.release = None;
    }
}

impl Drop for RunLeaseGuard {
    fn drop(&mut self) {
        if let Some(h) = self.heartbeat.take() {
            h.abort();
        }
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
    lease_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

async fn run_or_preview(
    db: Option<Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    response_cap: usize,
    execute: bool,
    ctx: Option<RunContext>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
) -> Result<LifecycleRunOutcome, String> {
    // retain-newest is SET-RELATIVE (it ranks the whole prefix) and takes a
    // dedicated collect→rank→act path — it must never touch the per-object age
    // machinery below (which would parse a nonexistent expire_after and apply a
    // per-object decision that can't express "keep newest N").
    if let LifecycleAction::RetainNewest(action) = &rule.action {
        return run_or_preview_retain_newest(
            db,
            engine,
            rule,
            action,
            response_cap,
            execute,
            ctx,
            maintenance_gate,
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
    let mut out = LifecycleRunOutcome {
        run_id: ctx.as_ref().and_then(|c| c.run_id),
        rule_name: rule.name.clone(),
        status: if execute { "succeeded" } else { "preview" }.to_string(),
        ..LifecycleRunOutcome::default()
    };

    'pages: while let Some(page_idx) = pager.begin_page() {
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
            break 'pages;
        }
        if execute
            && !renew_run_lease(&db, rule, ctx.as_ref(), &mut out.failures, response_cap).await?
        {
            out.errors += 1;
            break 'pages;
        }

        let page = engine
            .list_objects(&rule.bucket, &prefix, None, page_size, pager.token(), true)
            .await
            .map_err(|err| format!("list lifecycle page {page_idx} failed: {err}"));
        let page = match page {
            Ok(page) => page,
            Err(err) if execute => {
                // Poison-token guard: if the FIRST page of a RESUMED run
                // fails to list, the persisted cursor itself is the prime
                // suspect (backends invalidate tokens). Clear it so the
                // next run starts fresh instead of failing forever.
                if pager.poisoned_resume_token() {
                    if let Some(db) = db.as_ref() {
                        let db = db.lock().await;
                        let _ =
                            db.lifecycle_set_continuation_token(&rule.name, None, &cursor_scope);
                    }
                }
                out.errors += 1;
                let msg = err.to_string();
                push_failure(&mut out.failures, response_cap, String::new(), msg.clone());
                record_failure(&db, rule, ctx.as_ref(), "", &msg).await?;
                break 'pages;
            }
            Err(err) => return Err(err),
        };

        out.objects_scanned += page.objects.len() as i64;

        for (key, meta) in page.objects {
            match plan_object(
                rule,
                &key,
                &meta,
                expire_before,
                &include_globs,
                &exclude_globs,
            ) {
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
                        });
                    }
                    if execute {
                        match execute_action(db.as_ref(), engine, rule, &key, &meta, &action).await
                        {
                            Ok(ActionOutcome::Acted(bytes_actioned)) => {
                                out.objects_affected += 1;
                                out.bytes_affected += bytes_actioned as i64;
                            }
                            Ok(ActionOutcome::Skipped) => {
                                out.objects_skipped += 1;
                            }
                            Err(err) => {
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
                    }
                }
            }
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

    if out.errors > 0 {
        out.status = "failed".to_string();
    }
    Ok(out)
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
    db: Option<Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    action: &LifecycleRetainNewestAction,
    response_cap: usize,
    execute: bool,
    ctx: Option<RunContext>,
    maintenance_gate: Option<Arc<crate::maintenance::gate::MaintenanceGate>>,
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
        // Mid-run maintenance defer: a write bucket (source or transition dest)
        // became gated mid-run. Abort — the next scheduled run re-collects the
        // WHOLE set once maintenance clears. execute-only (preview never writes).
        if execute && maintenance_write_bucket_busy(rule, maintenance_gate.as_ref()) {
            info!(
                "lifecycle rule '{}' deferring mid-run — a write bucket is under maintenance",
                rule.name
            );
            incomplete_collect = Some("a write bucket is under maintenance");
            break 'pages;
        }
        if execute
            && !renew_run_lease(&db, rule, ctx.as_ref(), &mut out.failures, response_cap).await?
        {
            out.errors += 1;
            incomplete_collect = Some("replication lease lost");
            break 'pages;
        }
        let page = engine
            .list_objects(&rule.bucket, &prefix, None, page_size, pager.token(), true)
            .await
            .map_err(|err| format!("list lifecycle page {page_idx} failed: {err}"))?;

        out.objects_scanned += page.objects.len() as i64;
        for (key, meta) in page.objects {
            // Same structural guards as the age path (the pure plan_object's
            // first three checks). Globs/internal/marker filtering happens HERE,
            // BEFORE the size/age qualify ranking in plan_retain_newest.
            if key.ends_with('/') || is_internal_key(&key) {
                out.objects_skipped += 1;
                continue;
            }
            if exclude_globs.is_match(&key) {
                out.objects_skipped += 1;
                continue;
            }
            if !include_globs.is_empty() && !include_globs.is_match(&key) {
                out.objects_skipped += 1;
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
        });
    }

    // ── Act phase ──
    if !execute {
        out.objects_affected = plan.delete.len() as i64;
        out.bytes_affected = plan.delete.iter().map(|c| c.size as i64).sum();
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

    for c in &plan.delete {
        // TOCTOU guard: the ranking used a snapshot taken during collect. An
        // overwrite since then is a new object that might rank in the KEEP set.
        let snapshot = match metas.get(&c.key) {
            Some(meta) => Snapshot::of(meta),
            None => Snapshot {
                created_at: c.created_at,
                etag: String::new(),
                size: c.size,
            },
        };
        match recheck_before_delete(engine, &rule.bucket, &c.key, &snapshot).await {
            DeleteCheck::Proceed => {}
            DeleteCheck::Changed | DeleteCheck::Gone => {
                out.objects_skipped += 1;
                debug!(
                    "lifecycle rule '{}': retain-newest skipping {:?} — changed or gone since collect",
                    rule.name, c.key
                );
                continue;
            }
            DeleteCheck::HeadFailed(msg) => {
                out.errors += 1;
                push_failure(&mut out.failures, response_cap, c.key.clone(), msg.clone());
                record_failure(&db, rule, ctx.as_ref(), &c.key, &msg).await?;
                continue;
            }
        }

        let meta = metas.get(&c.key);
        match engine.delete(&rule.bucket, &c.key).await {
            Ok(_) => {
                out.objects_affected += 1;
                out.bytes_affected += c.size as i64;
                if let Some(meta) = meta {
                    append_lifecycle_delete_event(db.as_ref(), rule, &c.key, meta, "retain-newest")
                        .await;
                }
            }
            Err(err) => {
                out.errors += 1;
                let msg = err.to_string();
                push_failure(&mut out.failures, response_cap, c.key.clone(), msg.clone());
                record_failure(&db, rule, ctx.as_ref(), &c.key, &msg).await?;
            }
        }
    }

    if out.errors > 0 {
        out.status = "failed".to_string();
    }
    Ok(out)
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

/// Every lifecycle delete goes through this re-HEAD: the plan was made on a
/// listing snapshot, and a delete by key would remove a newer overwrite.
/// (A small window between this HEAD and the delete remains: the engine has
/// no conditional delete.)
async fn recheck_before_delete(
    engine: &DynEngine,
    bucket: &str,
    key: &str,
    snapshot: &Snapshot,
) -> DeleteCheck {
    classify_delete_check(snapshot, engine.head(bucket, key).await.as_ref())
}

/// What `execute_action` did with one planned object.
#[derive(Debug, PartialEq, Eq)]
enum ActionOutcome {
    /// Deleted and/or copied this many bytes.
    Acted(u64),
    /// The object changed or vanished since the listing: nothing done.
    Skipped,
}

async fn execute_action(
    db: Option<&Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    action: &PlannedLifecycleAction,
) -> Result<ActionOutcome, Box<dyn std::error::Error + Send + Sync>> {
    match action {
        PlannedLifecycleAction::Delete => {
            match recheck_before_delete(engine, &rule.bucket, key, &Snapshot::of(meta)).await {
                DeleteCheck::Proceed => {}
                DeleteCheck::Changed | DeleteCheck::Gone => return Ok(ActionOutcome::Skipped),
                DeleteCheck::HeadFailed(msg) => return Err(msg.into()),
            }
            engine.delete(&rule.bucket, key).await?;
            append_lifecycle_delete_event(db, rule, key, meta, "delete").await;
            Ok(ActionOutcome::Acted(meta.file_size))
        }
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
                        return Ok(ActionOutcome::Skipped);
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
                },
            )
            .await?;
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

            if *delete_source_after_success {
                match recheck_before_delete(engine, &rule.bucket, key, &Snapshot::of(meta)).await {
                    DeleteCheck::Proceed => {
                        engine.delete(&rule.bucket, key).await?;
                        append_lifecycle_delete_event(
                            db,
                            rule,
                            key,
                            meta,
                            "transition-source-delete",
                        )
                        .await;
                    }
                    // Overwritten since the listing: the copy is done, but the
                    // new source is not ours to delete.
                    DeleteCheck::Changed | DeleteCheck::Gone => {}
                    DeleteCheck::HeadFailed(msg) => return Err(msg.into()),
                }
            }

            Ok(ActionOutcome::Acted(copied.bytes_copied as u64))
        }
    }
}

fn spawn_lease_heartbeat(
    db: Option<Arc<Mutex<ConfigDb>>>,
    rule_name: &str,
    lease: Option<RunLease>,
    lease_alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let db = db?;
    let lease = lease?;
    let rule_name = rule_name.to_string();
    let heartbeat_secs = lease.heartbeat_secs.max(1) as u64;
    Some(tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(heartbeat_secs);
        let mut last_ok = std::time::Instant::now();
        loop {
            tokio::time::sleep(interval).await;
            let renewed = {
                let db = db.lock().await;
                db.lifecycle_renew_lease(
                    &rule_name,
                    &lease.owner,
                    super::current_unix_seconds(),
                    lease.ttl_secs,
                )
            };
            match heartbeat_step(&renewed, last_ok.elapsed(), &lease) {
                HeartbeatStep::Renewed => last_ok = std::time::Instant::now(),
                HeartbeatStep::Retry => {
                    if let Err(e) = &renewed {
                        warn!(
                            "Lifecycle lease renew for rule '{rule_name}' failed ({e}); retrying"
                        );
                    }
                }
                HeartbeatStep::Lost => {
                    lease_alive.store(false, std::sync::atomic::Ordering::Release);
                    warn!(
                        "Lifecycle lease heartbeat lost for rule '{}'; worker will stop before more work",
                        rule_name
                    );
                    return;
                }
            }
        }
    }))
}

#[derive(Debug, PartialEq, Eq)]
enum HeartbeatStep {
    Renewed,
    Retry,
    Lost,
}

/// One heartbeat's verdict. A refused renew is a lost lease. A DB error is
/// not: the lease is still ours until its TTL, so retry while the next
/// retry still lands before the expiry (`since_ok` = age of the last renew).
fn heartbeat_step<E>(
    renewed: &Result<bool, E>,
    since_ok: std::time::Duration,
    lease: &RunLease,
) -> HeartbeatStep {
    match renewed {
        Ok(true) => HeartbeatStep::Renewed,
        Ok(false) => HeartbeatStep::Lost,
        Err(_) => {
            let ttl = lease.ttl_secs.max(1) as u64;
            let hb = lease.heartbeat_secs.max(1) as u64;
            if since_ok.as_secs().saturating_add(hb) < ttl {
                HeartbeatStep::Retry
            } else {
                HeartbeatStep::Lost
            }
        }
    }
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
    db: &Option<Arc<Mutex<ConfigDb>>>,
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
    // Lock the DB BEFORE checking lease_alive so the check and the renewal are
    // ordered against the heartbeat task, which sets lease_alive under the same
    // lock when its own renewal fails. Without this, the heartbeat could declare
    // the lease lost between an early flag load and acquiring the lock here, and
    // we'd renew a lease the heartbeat already gave up on.
    let renewed = {
        let guard = db.lock().await;
        if !ctx.lease_alive.load(std::sync::atomic::Ordering::Acquire) {
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
    db: &Option<Arc<Mutex<ConfigDb>>>,
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

async fn append_lifecycle_delete_event(
    db: Option<&Arc<Mutex<ConfigDb>>>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    action: &str,
) {
    let Some(db) = db else {
        return;
    };
    let event = NewEvent::new(
        EventKind::LifecycleExpired,
        rule.bucket.as_str(),
        key,
        EventSource::Lifecycle,
        super::current_unix_seconds(),
        serde_json::json!({
            "rule_name": &rule.name,
            "action": action,
            "expire_after": &rule.expire_after,
            "created_at": meta.created_at.to_rfc3339(),
            "content_length": meta.file_size,
        }),
    );
    let db = db.lock().await;
    if let Err(err) = db.event_outbox_insert(&event) {
        warn!(
            "lifecycle rule '{}' could not append delete event for {:?}: {}",
            rule.name, key, err
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn append_lifecycle_transition_event(
    db: Option<&Arc<Mutex<ConfigDb>>>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    destination_bucket: &str,
    destination_key: &str,
    content_length: u64,
    delete_source_after_success: bool,
) {
    let Some(db) = db else {
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
    use crate::storage::{FilesystemBackend, StorageBackend};
    use std::sync::Arc;

    async fn fs_engine(dir: &std::path::Path) -> Arc<DynEngine> {
        let backend: Box<dyn StorageBackend> =
            Box::new(FilesystemBackend::new(dir.to_path_buf()).await.unwrap());
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
    use crate::storage::{FilesystemBackend, StorageBackend};

    /// The run's lease heartbeat is a detached task, aborted only on the
    /// normal return. A run that unwinds (panic in the spawned run-now task)
    /// or whose future is dropped leaves the heartbeat renewing the lease
    /// forever: run-now, the scheduler and rule delete are refused until a
    /// restart. The future drop stands in for the unwind here.
    #[tokio::test]
    async fn review3_an_aborted_run_does_not_keep_its_lease_alive() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<dyn StorageBackend> = Box::new(
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
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("k").unwrap()));
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
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("k").unwrap()));
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
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let hb = spawn_lease_heartbeat(
            Some(db.clone()),
            "r",
            Some(RunLease {
                owner: "X".into(),
                ttl_secs: 30,
                heartbeat_secs: 1,
            }),
            alive.clone(),
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        db.lock()
            .await
            .conn
            .execute_batch("ALTER TABLE lifecycle_state_off RENAME TO lifecycle_state")
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        hb.abort();
        assert!(
            alive.load(std::sync::atomic::Ordering::Acquire),
            "one failed renew marked a live lease lost"
        );
    }

    #[test]
    fn heartbeat_step_truth_table() {
        let lease = RunLease {
            owner: "X".into(),
            ttl_secs: 300,
            heartbeat_secs: 60,
        };
        let secs = std::time::Duration::from_secs;
        let err: Result<bool, &str> = Err("busy");
        assert_eq!(
            heartbeat_step(&Ok::<_, ()>(true), secs(999), &lease),
            HeartbeatStep::Renewed
        );
        assert_eq!(
            heartbeat_step(&Ok::<_, ()>(false), secs(0), &lease),
            HeartbeatStep::Lost
        );
        assert_eq!(heartbeat_step(&err, secs(60), &lease), HeartbeatStep::Retry);
        assert_eq!(
            heartbeat_step(&err, secs(239), &lease),
            HeartbeatStep::Retry
        );
        // The next retry would land at the expiry: lost now.
        assert_eq!(heartbeat_step(&err, secs(240), &lease), HeartbeatStep::Lost);
    }
}
