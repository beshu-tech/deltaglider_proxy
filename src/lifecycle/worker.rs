//! Lifecycle execution through the DeltaGlider engine.

use super::planner::{
    compile_rule_globs, lifecycle_prefix, plan_object, Decision, PlannedLifecycleAction,
    SkipReason, MAX_PAGES_PER_RUN,
};
use super::state_store::{LifecycleFailureInsert, LifecycleRunTotals};
use crate::config_db::ConfigDb;
use crate::config_sections::LifecycleRule;
use crate::deltaglider::DynEngine;
use crate::event_outbox::{EventKind, EventSource, NewEvent};
use crate::transfer::{
    copy_object_with_retries, ObjectTransferRequest, TransferProvenance,
    LIFECYCLE_RULE_METADATA_KEY,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

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
    pub candidates: Vec<PreviewObject>,
    pub failures: Vec<LifecycleFailure>,
}

#[derive(Debug, Clone)]
pub struct RunLease {
    pub owner: String,
    pub ttl_secs: i64,
    pub heartbeat_secs: i64,
}

pub async fn preview_rule(
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_candidates: usize,
) -> Result<LifecycleRunOutcome, String> {
    run_or_preview(None, engine, rule, max_candidates, false, None).await
}

pub async fn run_rule(
    db: Option<Arc<Mutex<ConfigDb>>>,
    engine: &Arc<DynEngine>,
    rule: &LifecycleRule,
    max_failures_retained: u32,
    triggered_by: &str,
    next_due_delay_secs: i64,
    lease: Option<RunLease>,
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
) -> Result<LifecycleRunOutcome, String> {
    let expire_after = humantime::parse_duration(&rule.expire_after)
        .map_err(|err| format!("expire_after={} invalid: {}", rule.expire_after, err))?;
    let expire_after = ChronoDuration::from_std(expire_after)
        .map_err(|err| format!("expire_after={} out of range: {}", rule.expire_after, err))?;
    let expire_before = Utc::now() - expire_after;
    let (include_globs, exclude_globs) = compile_rule_globs(rule).map_err(|err| err.to_string())?;
    let prefix = lifecycle_prefix(rule);
    let page_size = rule.batch_size.clamp(1, 10_000);
    let mut cursor: Option<String> = None;
    let mut out = LifecycleRunOutcome {
        run_id: ctx.as_ref().and_then(|c| c.run_id),
        rule_name: rule.name.clone(),
        status: if execute { "succeeded" } else { "preview" }.to_string(),
        ..LifecycleRunOutcome::default()
    };

    'pages: for page_idx in 0..MAX_PAGES_PER_RUN {
        if execute
            && !renew_run_lease(&db, rule, ctx.as_ref(), &mut out.failures, response_cap).await?
        {
            out.errors += 1;
            break 'pages;
        }

        let page = engine
            .list_objects(
                &rule.bucket,
                &prefix,
                None,
                page_size,
                cursor.as_deref(),
                true,
            )
            .await
            .map_err(|err| format!("list lifecycle page {page_idx} failed: {err}"));
        let page = match page {
            Ok(page) => page,
            Err(err) if execute => {
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

        cursor = page.next_continuation_token;
        if !page.is_truncated || cursor.is_none() {
            break 'pages;
        }
    }

    if out.errors > 0 {
        out.status = "failed".to_string();
    }
    Ok(out)
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
                    operation: "lifecycle transition",
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
    let lost = !ctx.lease_alive.load(std::sync::atomic::Ordering::Acquire);
    let renewed = if lost {
        false
    } else {
        let guard = db.lock().await;
        guard
            .lifecycle_renew_lease(
                &rule.name,
                &lease.owner,
                super::current_unix_seconds(),
                lease.ttl_secs,
            )
            .map_err(|err| err.to_string())?
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
