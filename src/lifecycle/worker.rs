// SPDX-License-Identifier: GPL-3.0-only

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
    let started_at = super::current_unix_seconds();
    let run_id = if let Some(db) = db.as_ref() {
        let db = db.lock().await;
        db.lifecycle_ensure_state(&rule.name, started_at)
            .map_err(|err| err.to_string())?;
        Some(
            db.lifecycle_begin_run(&rule.name, started_at, triggered_by)
                .map_err(|err| err.to_string())?,
        )
    } else {
        None
    };

    let lease_alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat_handle =
        spawn_lease_heartbeat(db.clone(), &rule.name, lease.clone(), lease_alive.clone());

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
    if let Some(handle) = heartbeat_handle {
        handle.abort();
    }

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
                            Ok(bytes_actioned) => {
                                out.objects_affected += 1;
                                out.bytes_affected += bytes_actioned as i64;
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

    for c in &plan.delete {
        // TOCTOU guard: the ranking used a snapshot taken during collect. If the
        // object was OVERWRITTEN between collect and now (same key, newer
        // created_at), the snapshot is stale — re-ranked, the fresh object might
        // be in the KEEP set. Re-HEAD and skip the delete on a created_at change
        // so a concurrent backup upload can never be deleted out from under the
        // client. (A delete by another writer surfaces as NotFound → already gone.)
        match engine.head(&rule.bucket, &c.key).await {
            Ok(current) if current.created_at != c.created_at => {
                out.objects_skipped += 1;
                debug!(
                    "lifecycle rule '{}': retain-newest skipping {:?} — changed since collect \
                     (ranked {} → current {})",
                    rule.name,
                    c.key,
                    c.created_at.to_rfc3339(),
                    current.created_at.to_rfc3339()
                );
                continue;
            }
            // Already gone (deleted by another writer) — nothing to do.
            Err(crate::deltaglider::EngineError::NotFound(_)) => {
                out.objects_skipped += 1;
                continue;
            }
            // Unchanged, or HEAD failed for another reason: fall through to the
            // delete, which re-resolves metadata and reports its own errors.
            _ => {}
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

async fn execute_action(
    db: Option<&Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    key: &str,
    meta: &crate::types::FileMetadata,
    action: &PlannedLifecycleAction,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    match action {
        PlannedLifecycleAction::Delete => {
            engine.delete(&rule.bucket, key).await?;
            append_lifecycle_delete_event(db, rule, key, meta, "delete").await;
            Ok(meta.file_size)
        }
        PlannedLifecycleAction::Transition {
            destination_bucket,
            destination_key,
            delete_source_after_success,
        } => {
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
                copied.bytes_copied,
                *delete_source_after_success,
            )
            .await;

            if *delete_source_after_success {
                engine.delete(&rule.bucket, key).await?;
                append_lifecycle_delete_event(db, rule, key, meta, "transition-source-delete")
                    .await;
            }

            Ok(copied.bytes_copied as u64)
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
                .unwrap_or(false)
            };
            if !renewed {
                lease_alive.store(false, std::sync::atomic::Ordering::Release);
                warn!(
                    "Lifecycle lease heartbeat lost for rule '{}'; worker will stop before more work",
                    rule_name
                );
                return;
            }
        }
    }))
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
            guard
                .lifecycle_renew_lease(
                    &rule.name,
                    &lease.owner,
                    super::current_unix_seconds(),
                    lease.ttl_secs,
                )
                .map_err(|err| err.to_string())?
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
    bytes_copied: usize,
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
            "content_length": bytes_copied,
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
