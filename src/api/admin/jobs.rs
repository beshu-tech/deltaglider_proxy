// SPDX-License-Identifier: BUSL-1.1

//! Unified jobs API — ONE read+action surface over the three job
//! subsystems (replication rules, lifecycle rules, maintenance one-offs).
//!
//! The operator-facing model is a single concept: a **job** with a kind
//! (`replication` / `lifecycle` / `reencrypt` / `migrate`), a scope, a
//! trigger (`continuous` / `scheduled` / `oneoff`), a normalized status,
//! progress, runs, and failures. Per-kind action semantics stay in the
//! per-subsystem handlers; this module routes to them by job id.
//!
//! Routes (admin tier unless noted):
//! - `GET  /_/api/admin/jobs` — every job, one row shape.
//! - `GET  /_/api/admin/jobs/:id/runs` · `GET …/failures`
//! - `POST /_/api/admin/jobs/:id/{pause,resume,run-now,preview,cancel}`
//! - `POST /_/api/admin/jobs/reencrypt` — create re-encrypt jobs.
//! - `GET  /_/api/admin/jobs/bucket/:bucket` — SESSION-LIGHT (browser
//!   busy banner; non-admin sessions included).
//!
//! Job ids are `"<subsystem>:<key>"` — `replication:<rule>`,
//! `lifecycle:<rule>`, `maintenance:<row-id>`. The id names the
//! SUBSYSTEM (stable even as maintenance grows more kinds); `kind` is
//! reported separately.

use crate::api::admin::extract::AdminQuery;
use crate::coordination::CoordinationLease;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{AdminError, AdminState};
use crate::maintenance::migrate;
use crate::maintenance::store::MaintenanceJob;

// ─────────────────────────── pure decision logic ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobSubsystem {
    Replication,
    Lifecycle,
    Maintenance,
}

/// Parse `"replication:nightly"` / `"lifecycle:expire"` /
/// `"maintenance:42"` (the numeric key is validated for maintenance).
pub fn parse_job_id(id: &str) -> Option<(JobSubsystem, &str)> {
    let (sub, key) = id.split_once(':')?;
    if key.is_empty() {
        return None;
    }
    match sub {
        "replication" => Some((JobSubsystem::Replication, key)),
        "lifecycle" => Some((JobSubsystem::Lifecycle, key)),
        "maintenance" => key
            .parse::<i64>()
            .ok()
            .map(|_| (JobSubsystem::Maintenance, key)),
        _ => None,
    }
}

/// One status vocabulary:
/// `idle | queued | running | cancelling | paused | deferred | succeeded |
/// completed_with_errors | failed | cancelled`.
/// The raw subsystem value is reported alongside (`status_raw`); unknown
/// raw values normalize to `idle` (total function — never panics).
///
/// `completed_with_errors` is its OWN normalized state, distinct from both
/// `succeeded` and `failed`: the sweep finished but ≥1 object errored (a
/// transient destination 500, say). Collapsing it into `failed` made healthy
/// runs that copied thousands of objects look broken.
pub fn normalize_status(raw: &str) -> &'static str {
    match raw {
        "idle" => "idle",
        "queued" => "queued",
        "running" => "running",
        "cancelling" => "cancelling",
        "succeeded" | "completed" => "succeeded",
        "completed_with_errors" => "completed_with_errors",
        "failed" => "failed",
        "cancelled" => "cancelled",
        // A run that an operator pause stopped: its cursor is kept and the
        // next run continues from it — not the same as a kill (`cancelled`).
        "paused" => "paused",
        // A run that the proxy stopped (a maintenance job or a failed health
        // check gates its bucket; `stopped`: a replication pass ran out of
        // its page budget). Nobody paused or killed it: its cursor is kept,
        // and the next run, about a minute later, continues from it.
        "deferred" | "stopped" => "deferred",
        _ => "idle",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    Pause,
    Resume,
    RunNow,
    Preview,
    Cancel,
    Verify,
    Delete,
    Kill,
}

impl JobAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pause" => Some(Self::Pause),
            "resume" => Some(Self::Resume),
            "run-now" => Some(Self::RunNow),
            "preview" => Some(Self::Preview),
            "cancel" => Some(Self::Cancel),
            "verify" => Some(Self::Verify),
            "delete" => Some(Self::Delete),
            "kill" => Some(Self::Kill),
            _ => None,
        }
    }

    /// The wire name (inverse of [`parse`]) — used in user-facing errors,
    /// never Debug formatting.
    ///
    /// [`parse`]: Self::parse
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::RunNow => "run-now",
            Self::Preview => "preview",
            Self::Cancel => "cancel",
            Self::Verify => "verify",
            Self::Delete => "delete",
            Self::Kill => "kill",
        }
    }
}

/// The uniform capability matrix — what each subsystem's jobs support.
pub fn supported_actions(sub: JobSubsystem) -> &'static [JobAction] {
    match sub {
        JobSubsystem::Replication => &[
            JobAction::Pause,
            JobAction::Resume,
            JobAction::RunNow,
            JobAction::Verify,
            JobAction::Delete,
            JobAction::Kill,
        ],
        JobSubsystem::Lifecycle => &[
            JobAction::Pause,
            JobAction::Resume,
            JobAction::RunNow,
            JobAction::Preview,
            JobAction::Delete,
            JobAction::Kill,
        ],
        JobSubsystem::Maintenance => &[JobAction::Cancel, JobAction::Delete],
    }
}

// ─────────────────────────── response shapes ───────────────────────────

#[derive(Debug, Serialize)]
pub struct JobScope {
    pub bucket: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct JobProgress {
    pub processed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    pub bytes: i64,
    pub failed: i64,
    pub skipped: i64,
    /// Objects the job deleted (replication mirror deletes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<i64>,
    /// Lifecycle: of `skipped`, the objects an include/exclude glob keeps
    /// out of the rule. `None` when the run did not store the split.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded: Option<i64>,
    /// Lifecycle: of `skipped`, the objects not old enough yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_due: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct JobLifetime {
    pub objects: i64,
    pub bytes: i64,
}

#[derive(Debug, Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub scope: JobScope,
    pub trigger: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    pub status: &'static str,
    pub status_raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    pub progress: JobProgress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifetime: Option<JobLifetime>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_due_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Failures recorded for the latest run (the ring may keep fewer).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_total: Option<i64>,
    pub detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct JobsOverview {
    pub workers: serde_json::Value,
    pub jobs: Vec<JobView>,
    /// Finished one-off jobs older than the ones listed (`GET /jobs/:id`
    /// still answers for them). Omitted when 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub older_oneoffs: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Unified run entry. Maintenance jobs synthesize ONE run from the job
/// row (a one-off job IS its run) so the frontend loop stays uniform.
#[derive(Debug, Serialize)]
pub struct JobRunEntry {
    pub id: i64,
    pub triggered_by: String,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    pub status: &'static str,
    pub status_raw: String,
    pub objects_scanned: i64,
    pub objects_processed: i64,
    pub objects_skipped: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objects_deleted: Option<i64>,
    pub bytes: i64,
    pub errors: i64,
    /// Objects that shipped their `.delta` verbatim on the fast path
    /// (replication only; 0 for other subsystems). The rest = copied −
    /// delta_passthrough. Egress saved = Σ(logical − delta).
    pub delta_passthrough: i64,
    pub bytes_egress_saved: i64,
    /// Objects decompressed + re-stored (recompress/re-encrypt). Straight
    /// passthrough = copied − delta_passthrough − reconstructed.
    pub reconstructed: i64,
}

/// Unified failure entry — field union; `object_key` is always set
/// (replication mirrors its source_key into it).
#[derive(Debug, Serialize)]
pub struct JobFailureEntry {
    pub id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub object_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_key: Option<String>,
    pub error: String,
    /// What recorded the failure: `run` (a run of the rule) or `event` (the
    /// event-driven path, which copies between runs). Replication only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<&'static str>,
}

/// The scope of a replication rule: its source, and its destination bucket.
pub(super) fn replication_scope(rule: &crate::config_sections::ReplicationRule) -> JobScope {
    JobScope {
        bucket: rule.source.bucket.clone(),
        prefix: (!rule.source.prefix.is_empty()).then(|| rule.source.prefix.clone()),
        target: Some(rule.destination.bucket.clone()),
    }
}

/// The scope of a lifecycle rule: its bucket and prefix, and where a
/// transition moves objects.
pub(super) fn lifecycle_scope(rule: &crate::config_sections::LifecycleRule) -> JobScope {
    JobScope {
        bucket: rule.bucket.clone(),
        prefix: (!rule.prefix.is_empty()).then(|| rule.prefix.clone()),
        target: match &rule.action {
            crate::config_sections::LifecycleAction::Transition(t) => {
                Some(t.destination.bucket.clone())
            }
            _ => None,
        },
    }
}

/// PURE: the live progress of a replication rule's row, from its newest run
/// (its counters are written at each checkpoint while it runs). No run yet:
/// zeros. `bytes` is the logical size of the objects copied.
pub(super) fn replication_row_progress(
    run: Option<&crate::replication::state_store::RunRecord>,
) -> JobProgress {
    match run {
        None => JobProgress::default(),
        Some(r) => JobProgress {
            processed: r.objects_copied,
            total: None,
            bytes: r.bytes_copied,
            failed: r.errors,
            skipped: r.objects_skipped,
            deleted: Some(r.objects_deleted),
            excluded: None,
            not_due: None,
        },
    }
}

pub(super) fn maintenance_job_view(j: &MaintenanceJob) -> JobView {
    let percent = crate::maintenance::display_percent(j);
    let (target, detail) = match (j.kind.as_str(), j.params.as_deref()) {
        ("migrate", Some(p)) => match migrate::parse_params(p) {
            Ok(mp) => (
                Some(mp.target_backend.clone()),
                serde_json::json!({
                    "target_backend": mp.target_backend,
                    "from_backend": mp.from_backend,
                    "delete_source": mp.delete_source,
                    "target": mp.target.as_str(),
                }),
            ),
            Err(_) => (None, serde_json::json!({})),
        },
        ("backfill-metadata", Some(p)) => match crate::maintenance::backfill::parse_params(p) {
            Ok(bp) => (
                None,
                serde_json::json!({ "refresh_last_modified": bp.refresh_last_modified }),
            ),
            Err(_) => (None, serde_json::json!({})),
        },
        (crate::maintenance::delete::KIND, Some(p)) => {
            match crate::maintenance::delete::parse_params(p) {
                // The folders by name; the objects selected directly by count
                // (up to 10,000 of them).
                Ok(dp) => (
                    None,
                    serde_json::json!({ "prefixes": dp.prefixes, "objects": dp.keys.len() }),
                ),
                Err(_) => (None, serde_json::json!({})),
            }
        }
        _ => (None, serde_json::json!({})),
    };
    JobView {
        id: format!("maintenance:{}", j.id),
        kind: j.kind.clone(),
        name: j.bucket.clone(),
        scope: JobScope {
            bucket: j.bucket.clone(),
            prefix: None,
            target,
        },
        trigger: "oneoff",
        enabled: None,
        paused: None,
        status: normalize_status(&j.status),
        status_raw: j.status.clone(),
        phase: maintenance_phase(j),
        percent,
        progress: JobProgress {
            processed: j.objects_done,
            total: j.objects_total,
            bytes: j.bytes_done,
            failed: j.objects_failed,
            skipped: j.objects_skipped,
            deleted: None,
            excluded: None,
            not_due: None,
        },
        lifetime: None,
        last_run_at: None,
        next_due_at: None,
        created_at: Some(j.created_at),
        started_at: j.started_at,
        finished_at: j.finished_at,
        last_error: j.last_error.clone(),
        // A one-off job is its one run: every failed object of it.
        failed_total: Some(j.objects_failed),
        detail,
    }
}

/// The Jobs row of a lifecycle rule. While a run of the rule runs in this
/// process, `progress` carries its live counters (the run-history row
/// gets its totals only at the end).
fn lifecycle_job_view(
    db: &crate::config_db::JobsDb,
    rule: &crate::config_sections::LifecycleRule,
    lc_cfg: &crate::config_sections::LifecycleConfig,
) -> JobView {
    let st = db.lifecycle_load_state(&rule.name).ok().flatten();
    let status_raw = st
        .as_ref()
        .map(|s| s.last_status.clone())
        .unwrap_or_else(|| "idle".to_string());
    let live = crate::lifecycle::live::running(&rule.name);
    let latest = db
        .lifecycle_recent_runs(&rule.name, 1)
        .ok()
        .and_then(|mut runs| runs.pop());
    let progress = match (&live, &latest) {
        (Some(run), _) => lifecycle_live_progress(&run.counters()),
        (None, Some(r)) => lifecycle_record_progress(r),
        (None, None) => JobProgress {
            deleted: Some(0),
            ..JobProgress::default()
        },
    };
    // The latest run's last failure, so a failed row says why.
    let (last_error, failed_total) = match &latest {
        Some(r) if r.errors > 0 => (
            db.lifecycle_last_failure_of_run(&rule.name, r.id)
                .ok()
                .flatten()
                .map(|f| lifecycle_failure_text(&f.object_key, &f.error_message)),
            Some(r.errors),
        ),
        _ => (None, None),
    };
    let started_at = live
        .as_ref()
        .map(|run| run.started_at)
        .or_else(|| latest.as_ref().map(|r| r.started_at));
    JobView {
        id: format!("lifecycle:{}", rule.name),
        kind: "lifecycle".into(),
        name: rule.name.clone(),
        scope: lifecycle_scope(rule),
        trigger: "scheduled",
        enabled: Some(rule.enabled),
        paused: Some(st.as_ref().map(|s| s.paused).unwrap_or(false)),
        status: normalize_status(&status_raw),
        status_raw,
        phase: None,
        percent: None,
        progress,
        lifetime: Some(JobLifetime {
            objects: st
                .as_ref()
                .map(|s| s.objects_affected_lifetime)
                .unwrap_or(0),
            bytes: st.as_ref().map(|s| s.bytes_affected_lifetime).unwrap_or(0),
        }),
        last_run_at: st.as_ref().and_then(|s| s.last_run_at),
        next_due_at: st.as_ref().map(|s| s.next_due_at),
        created_at: None,
        started_at,
        finished_at: if live.is_some() {
            None
        } else {
            latest.as_ref().and_then(|r| r.finished_at)
        },
        last_error,
        failed_total,
        detail: serde_json::json!({
            "action": rule.action.kind(),
            "expire_after": rule.expire_after,
            "retain_count": match &rule.action {
                crate::config_sections::LifecycleAction::RetainNewest(a) => Some(a.count),
                _ => None,
            },
            "delete_source_after_success": match &rule.action {
                crate::config_sections::LifecycleAction::Transition(t) => {
                    Some(t.delete_source_after_success)
                }
                _ => None,
            },
            "include_globs": rule.include_globs,
            "exclude_globs": rule.exclude_globs,
            // How often the rule runs: `next_due_at` is the end of its
            // last run plus this.
            "tick_interval": lc_cfg.tick_interval,
            // Why the scheduler did not run the due rule (a maintenance
            // job, or an unhealthy backend); absent when nothing holds it.
            "deferred": crate::lifecycle::live::deferred(&rule.name),
            "live": live.as_ref().map(|run| run.counters()),
        }),
    }
}

/// One lifecycle run entry: the shared shape, plus `objects_deleted`,
/// `objects_moved`, `objects_copied`, `objects_excluded` and
/// `objects_not_due`. The running run of this process shows its live
/// counters.
fn lifecycle_run_entry(
    r: crate::lifecycle::LifecycleRunRecord,
    live: Option<&crate::lifecycle::live::LiveRun>,
) -> serde_json::Value {
    let live = live
        .filter(|run| run.run_id == Some(r.id) && r.finished_at.is_none())
        .map(|run| run.counters());
    let c = match live {
        Some(c) => c,
        None => crate::lifecycle::live::LiveCounters {
            objects_scanned: r.objects_scanned,
            objects_affected: r.objects_affected,
            objects_skipped: r.objects_skipped,
            bytes_affected: r.bytes_affected,
            errors: r.errors,
            objects_deleted: r.split.map_or(0, |s| s.objects_deleted),
            objects_moved: r.split.map_or(0, |s| s.objects_moved),
            objects_copied: r.split.map_or(0, |s| s.objects_copied),
            objects_excluded: r.split.map_or(0, |s| s.objects_excluded),
            objects_not_due: r.split.map_or(0, |s| s.objects_not_due),
        },
    };
    // A run that ended before the split was stored: unknown, not 0.
    let known = live.is_some() || r.split.is_some();
    let entry = JobRunEntry {
        id: r.id,
        triggered_by: r.triggered_by,
        started_at: r.started_at,
        finished_at: r.finished_at,
        status: normalize_status(&r.status),
        status_raw: r.status,
        objects_scanned: c.objects_scanned,
        objects_processed: c.objects_affected,
        objects_skipped: c.objects_skipped,
        objects_deleted: known.then_some(c.objects_deleted),
        bytes: c.bytes_affected,
        errors: c.errors,
        delta_passthrough: 0,
        bytes_egress_saved: 0,
        reconstructed: 0,
    };
    let mut v = serde_json::to_value(entry).unwrap_or_default();
    if let (true, Some(obj)) = (known, v.as_object_mut()) {
        obj.insert("objects_moved".into(), c.objects_moved.into());
        obj.insert("objects_copied".into(), c.objects_copied.into());
        obj.insert("objects_excluded".into(), c.objects_excluded.into());
        obj.insert("objects_not_due".into(), c.objects_not_due.into());
    }
    v
}

/// The progress of a lifecycle run in progress in this process.
pub(super) fn lifecycle_live_progress(c: &crate::lifecycle::live::LiveCounters) -> JobProgress {
    JobProgress {
        processed: c.objects_affected,
        total: None,
        bytes: c.bytes_affected,
        failed: c.errors,
        skipped: c.objects_skipped,
        deleted: Some(c.objects_deleted),
        excluded: Some(c.objects_excluded),
        not_due: Some(c.objects_not_due),
    }
}

/// The progress of a lifecycle run from its history row. A run that ended
/// before the split was stored has no `excluded` / `not_due`.
pub(super) fn lifecycle_record_progress(r: &crate::lifecycle::LifecycleRunRecord) -> JobProgress {
    JobProgress {
        processed: r.objects_affected,
        total: None,
        bytes: r.bytes_affected,
        failed: r.errors,
        skipped: r.objects_skipped,
        deleted: Some(r.split.map_or(0, |s| s.objects_deleted)),
        excluded: r.split.map(|s| s.objects_excluded),
        not_due: r.split.map(|s| s.objects_not_due),
    }
}

/// A lifecycle failure as one line: the key, then the error.
fn lifecycle_failure_text(key: &str, error: &str) -> String {
    if key.is_empty() {
        error.to_string()
    } else {
        format!("{key}: {error}")
    }
}

/// The phase a one-off row reports: the phase it is in while active, the
/// phase it stopped in when it failed or was cancelled, none once it
/// completed (a finished migrate read "Phase: cleanup").
fn maintenance_phase(j: &MaintenanceJob) -> Option<String> {
    match j.status.as_str() {
        "completed" | "completed_with_errors" => None,
        _ => Some(j.phase.clone()),
    }
}

/// A one-off job IS its run: the one run entry of `GET /jobs/:id/runs`.
/// Scanned counts every object the job reached, failed ones included.
fn maintenance_run_entry(job: &MaintenanceJob) -> JobRunEntry {
    JobRunEntry {
        id: job.id,
        triggered_by: job.triggered_by.clone().unwrap_or_else(|| "admin".into()),
        started_at: job.started_at.unwrap_or(job.created_at),
        finished_at: job.finished_at,
        status: normalize_status(&job.status),
        status_raw: job.status.clone(),
        objects_scanned: job.objects_done + job.objects_skipped + job.objects_failed,
        objects_processed: job.objects_done,
        objects_skipped: job.objects_skipped,
        objects_deleted: None,
        bytes: job.bytes_done,
        errors: job.objects_failed,
        delta_passthrough: 0,
        bytes_egress_saved: 0,
        reconstructed: 0,
    }
}

/// One-off rows that `GET /jobs` lists: every active one and this many of
/// the newest. The answer counts the older ones it leaves out.
const ONEOFF_LIST_LIMIT: usize = 200;

// ─────────────────────────── handlers ───────────────────────────

/// GET /_/api/admin/jobs
pub async fn list_jobs(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<JobsOverview>, AdminError> {
    let cfg = state.config.read().await;
    let repl_cfg = cfg.replication.clone();
    let lc_cfg = cfg.lifecycle.clone();
    drop(cfg);

    let mut jobs: Vec<JobView> = Vec::new();
    let mut last_event_applied_at: Option<i64> = None;
    let mut older_oneoffs = 0i64;

    if let Some(db) = state.jobs_db.as_ref() {
        let db = db.lock().await;

        for rule in &repl_cfg.rules {
            // Read-only: absent state rows render as defaults below.
            // ensure_state belongs to the scheduler/run paths — doing it
            // here would put N writes behind every 2s UI poll.
            let st = db.replication_load_state(&rule.name).ok().flatten();
            let mut status_raw = st
                .as_ref()
                .map(|s| s.last_status.clone())
                .unwrap_or_else(|| "idle".to_string());
            // `replication_state.last_status` only settles when the worker
            // FINISHES a run. A kill flips the run-history row to `cancelling`
            // immediately, but the worker may not observe it for a while (e.g.
            // wedged in the planning phase). Surface the live history status so a
            // killed run shows `cancelling`, not a stale `running`.
            // The newest run: its live counters (written at each checkpoint)
            // are the row's progress, and its failures the row's error.
            let latest = db.replication_latest_run(&rule.name).ok().flatten();
            if status_raw == "running" {
                if let Some(live) = latest.as_ref().map(|r| r.status.as_str()) {
                    if live == "cancelling" {
                        status_raw = live.to_string();
                    }
                }
            }
            let (failed_total, last_error) = match latest.as_ref() {
                Some(r) => db
                    .replication_run_failure_summary(&rule.name, r.id)
                    .unwrap_or((0, None)),
                None => (0, None),
            };
            let events = crate::replication::event_consumer::event_activity(&rule.name);
            jobs.push(JobView {
                id: format!("replication:{}", rule.name),
                kind: "replication".into(),
                name: rule.name.clone(),
                scope: replication_scope(rule),
                trigger: "continuous",
                enabled: Some(rule.enabled),
                paused: Some(st.as_ref().map(|s| s.paused).unwrap_or(false)),
                status: normalize_status(&status_raw),
                status_raw,
                phase: None,
                percent: None,
                progress: replication_row_progress(latest.as_ref()),
                lifetime: Some(JobLifetime {
                    objects: st.as_ref().map(|s| s.objects_copied_lifetime).unwrap_or(0),
                    bytes: st.as_ref().map(|s| s.bytes_copied_lifetime).unwrap_or(0),
                }),
                last_run_at: st.as_ref().and_then(|s| s.last_run_at),
                next_due_at: st.as_ref().map(|s| s.next_due_at),
                created_at: None,
                started_at: latest.as_ref().map(|r| r.started_at),
                finished_at: latest.as_ref().and_then(|r| r.finished_at),
                last_error,
                failed_total: Some(failed_total),
                detail: serde_json::json!({
                    "interval": rule.interval,
                    "destination_prefix": rule.destination.prefix,
                    // What the event-driven path did between runs, since the
                    // proxy started (node-local): copies and deletes it
                    // applied, its failures, and when it last applied one.
                    "last_event_applied_at": events.as_ref().and_then(|e| e.last_applied_at),
                    "events": events,
                    // Live "currently copying" objects (largest first, top 3)
                    // so a slow-moving counter is explained in the UI — a
                    // 4 GB tarball at 10 MB/s is work, not a hang.
                    "in_flight": crate::replication::worker::inflight_snapshot(&rule.name)
                        .into_iter()
                        .take(3)
                        .collect::<Vec<_>>(),
                    // Live walk progress ("scanning ror/builds/… · 1,234 dirs
                    // done · 87 pending") for a running reconcile; absent when
                    // no walk is running. dirs_pending is NOT a percent basis
                    // (it grows-then-drains during tree discovery) — the UI
                    // shows it as an activity indicator, not a progress %.
                    "walk": crate::replication::worker::walk_snapshot(&rule.name)
                        .map(|w| serde_json::json!({
                            "dirs_completed": w.dirs_completed,
                            "dirs_pending": w.dirs_pending,
                            "scanning": w.scanning,
                        })),
                }),
            });
        }

        for rule in &lc_cfg.rules {
            jobs.push(lifecycle_job_view(&db, rule, &lc_cfg));
        }

        let oneoffs = db.maintenance_list_jobs(ONEOFF_LIST_LIMIT)?;
        older_oneoffs = (db.maintenance_count_jobs()? - oneoffs.len() as i64).max(0);
        for j in &oneoffs {
            jobs.push(maintenance_job_view(j));
        }

        last_event_applied_at = db
            .listener_cursor_load_full(crate::replication::event_consumer::REPLICATION_LISTENER)
            .ok()
            .flatten()
            .map(|c| c.updated_at);
    }

    Ok(Json(JobsOverview {
        workers: serde_json::json!({
            "replication": {
                "enabled": repl_cfg.enabled,
                "tick_interval": repl_cfg.tick_interval,
                "last_event_applied_at": last_event_applied_at,
            },
            "lifecycle": {
                "enabled": lc_cfg.enabled,
                "tick_interval": lc_cfg.tick_interval,
            },
        }),
        jobs,
        older_oneoffs,
    }))
}

/// GET /_/api/admin/jobs/:id — one job row. The list carries the most
/// recent one-offs only, so a link to an older one-off reads it here.
pub async fn get_job(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<JobView>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub == JobSubsystem::Maintenance {
        let job_id: i64 = key.parse().map_err(|_| not_found())?;
        let db = state
            .jobs_db
            .as_ref()
            .ok_or_else(AdminError::no_jobs_db)?
            .lock()
            .await;
        let job = db.maintenance_job_by_id(job_id)?.ok_or(not_found())?;
        return Ok(Json(maintenance_job_view(&job)));
    }
    // A rule row is built from config + state exactly as the list builds it.
    let Json(all) = list_jobs(State(state)).await?;
    all.jobs
        .into_iter()
        .find(|j| j.id == id)
        .map(Json)
        .ok_or(not_found())
}

/// Options of a job action. `fresh` (preview): never answer from a walk
/// that already ended.
#[derive(Debug, Default, Deserialize)]
pub struct ActionQuery {
    #[serde(default)]
    pub fresh: bool,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<u32>,
}

/// GET /_/api/admin/jobs/:id/verify — poll the server-side parity audit status
/// (replication only). No scan is started; the result survives navigation +
/// restart.
pub async fn job_verify_status(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let resp = super::replication::verify_status(state, key.to_string()).await?;
    Ok(Json(serde_json::to_value(resp).map_err(internal)?))
}

/// Options of `POST …/verify`. `scope=unresolved` re-reads only the objects
/// the last audit could not read; absent (or `all`) audits the whole mirror.
#[derive(Debug, Default, Deserialize)]
pub struct VerifyQuery {
    #[serde(default)]
    pub scope: super::replication::VerifyScope,
}

/// POST /_/api/admin/jobs/:id/verify — kick off the background parity audit
/// (replication only). Returns 202 + the running status. The literal `verify`
/// route carries this POST because it shadows the `:action` param at this path.
/// `?scope=unresolved` re-checks only the last audit's unread objects (409
/// when there are none, or an audit is running).
pub async fn job_verify_start(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<VerifyQuery>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<serde_json::Value>), AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let (code, resp) =
        super::replication::verify(state, key.to_string(), &headers, q.scope).await?;
    Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)))
}

/// POST /_/api/admin/jobs/:id/verify/cancel — cancel a running parity audit
/// (replication only).
pub async fn job_verify_cancel(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let resp = super::replication::verify_cancel(state, key.to_string()).await?;
    Ok(Json(serde_json::to_value(resp).map_err(internal)?))
}

/// GET /_/api/admin/jobs/:id/preview — the progress of the preview walk of a
/// lifecycle rule that runs now (`running: false` when none runs). The GUI
/// polls it while its `POST …/preview` waits. No walk is started.
pub async fn job_preview_status(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<crate::lifecycle::PreviewStatus>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Lifecycle {
        return Err(not_found());
    }
    let known = {
        let cfg = state.config.read().await;
        cfg.lifecycle.rules.iter().any(|r| r.name == key)
    };
    if !known {
        return Err(AdminError::not_found("rule not found"));
    }
    Ok(Json(crate::lifecycle::preview_status(key)))
}

/// POST /_/api/admin/jobs/:id/preview — the `preview` action. The literal
/// `preview` route carries the GET above, and a literal segment shadows the
/// `:action` param at this path, so it carries this POST too.
pub async fn job_preview(
    Path(id): Path<String>,
    state: State<Arc<AdminState>>,
    query: AdminQuery<ActionQuery>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    job_action(Path((id, "preview".to_string())), state, query, headers).await
}

/// GET /_/api/admin/jobs/parity-version — monotonic counter bumped each time a
/// background parity audit settles. Mirrors `iam/version`: lets integration
/// tests poll for a deterministic completion barrier instead of sleeping.
/// Unauthenticated (just a number, like the other version endpoints).
pub async fn job_parity_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "version": crate::replication::parity::current_parity_version() }))
}

/// GET /_/api/admin/jobs/replication-run-version — bumped each time a SCHEDULED
/// replication run settles. Sibling of parity-version; same test-barrier role.
pub async fn job_replication_run_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": crate::replication::state_store::current_replication_run_version()
    }))
}

/// GET /_/api/admin/jobs/replication-event-version — bumped each time an
/// EVENT-DRIVEN drain advances its cursor (event-driven runs write no run row).
pub async fn job_replication_event_version() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "version": crate::replication::state_store::current_replication_event_version()
    }))
}

/// GET /_/api/admin/jobs/:id/runs
pub async fn job_runs(
    Path(id): Path<String>,
    AdminQuery(q): AdminQuery<LimitQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .jobs_db
        .as_ref()
        .ok_or_else(AdminError::no_jobs_db)?
        .lock()
        .await;
    let runs: Vec<JobRunEntry> = match sub {
        JobSubsystem::Replication => db
            .replication_recent_runs(key, limit)?
            .into_iter()
            .map(|r| JobRunEntry {
                id: r.id,
                triggered_by: r.triggered_by,
                started_at: r.started_at,
                finished_at: r.finished_at,
                status: normalize_status(&r.status),
                status_raw: r.status,
                objects_scanned: r.objects_scanned,
                objects_processed: r.objects_copied,
                objects_skipped: r.objects_skipped,
                objects_deleted: Some(r.objects_deleted),
                bytes: r.bytes_copied,
                errors: r.errors,
                delta_passthrough: r.delta_passthrough,
                bytes_egress_saved: r.bytes_egress_saved,
                reconstructed: r.reconstructed,
            })
            .collect(),
        JobSubsystem::Lifecycle => {
            // Lifecycle entries carry more than the shared shape (the
            // copy/move split and why objects were skipped): answer here.
            let runs = db.lifecycle_recent_runs(key, limit)?;
            let live = crate::lifecycle::live::running(key);
            let runs: Vec<serde_json::Value> = runs
                .into_iter()
                .map(|r| lifecycle_run_entry(r, live.as_deref()))
                .collect();
            return Ok(Json(serde_json::json!({ "runs": runs })));
        }
        JobSubsystem::Maintenance => {
            let job_id: i64 = key.parse().map_err(|_| not_found())?;
            let job = db.maintenance_job_by_id(job_id)?.ok_or(not_found())?;
            vec![maintenance_run_entry(&job)]
        }
    };
    Ok(Json(serde_json::json!({ "runs": runs })))
}

/// Query of `GET /jobs/:id/failures`: how many, and optionally only the
/// failures of one run of a rule.
#[derive(Debug, Deserialize)]
pub struct FailuresQuery {
    pub limit: Option<u32>,
    pub run_id: Option<i64>,
}

/// GET /_/api/admin/jobs/:id/failures
pub async fn job_failures(
    Path(id): Path<String>,
    AdminQuery(q): AdminQuery<FailuresQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .jobs_db
        .as_ref()
        .ok_or_else(AdminError::no_jobs_db)?
        .lock()
        .await;
    let failures: Vec<JobFailureEntry> = match sub {
        JobSubsystem::Replication => db
            .replication_failures_of(key, q.run_id, limit)?
            .into_iter()
            .map(|f| JobFailureEntry {
                id: f.id,
                run_id: f.run_id,
                occurred_at: f.occurred_at,
                object_key: f.source_key.clone(),
                bucket: None,
                source_key: Some(f.source_key),
                dest_key: Some(f.dest_key),
                error: f.error_message,
                trigger: Some(if f.run_id.is_some() { "run" } else { "event" }),
            })
            .collect(),
        JobSubsystem::Lifecycle => db
            .lifecycle_failures_of(key, q.run_id, limit)?
            .into_iter()
            .map(|f| JobFailureEntry {
                id: f.id,
                run_id: f.run_id,
                occurred_at: f.occurred_at,
                object_key: f.object_key,
                bucket: Some(f.bucket),
                source_key: None,
                dest_key: None,
                error: f.error_message,
                trigger: None,
            })
            .collect(),
        JobSubsystem::Maintenance => {
            let job_id: i64 = key.parse().map_err(|_| not_found())?;
            db.maintenance_list_failures(job_id, limit as usize)?
                .into_iter()
                .map(|f| JobFailureEntry {
                    id: f.id,
                    run_id: None,
                    occurred_at: f.created_at,
                    object_key: f.object_key,
                    bucket: None,
                    source_key: None,
                    dest_key: None,
                    error: f.error,
                    trigger: None,
                })
                .collect()
        }
    };
    // How many failures there are in all, so the UI can say "20 most
    // recent of 500".
    let total = match sub {
        JobSubsystem::Replication => Some(db.replication_failure_count_of(key, q.run_id)?),
        JobSubsystem::Lifecycle => Some(db.lifecycle_failure_count_of(key, q.run_id)?),
        // A one-off row counts every failed object; the ring keeps the newest.
        JobSubsystem::Maintenance => key
            .parse::<i64>()
            .ok()
            .and_then(|id| db.maintenance_job_by_id(id).ok().flatten())
            .map(|j| j.objects_failed),
    };
    let mut body = serde_json::json!({ "failures": failures });
    if let Some(total) = total {
        body["total"] = total.into();
    }
    Ok(Json(body))
}

/// POST /_/api/admin/jobs/:id/:action — uniform action dispatch. A 204
/// has no body (source guard `no_content_responses_carry_no_body`).
pub async fn job_action(
    Path((id, action)): Path<(String, String)>,
    State(state): State<Arc<AdminState>>,
    AdminQuery(q): AdminQuery<ActionQuery>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let action = JobAction::parse(&action)
        .ok_or(AdminError::not_found(format!("unknown action '{action}'")))?;
    if !supported_actions(sub).contains(&action) {
        let available = supported_actions(sub)
            .iter()
            .map(|a| a.wire_name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AdminError::invalid(format!(
            "this action isn't available for this job kind (available: {available})"
        )));
    }
    let name = key.to_string();
    match (sub, action) {
        (JobSubsystem::Replication, JobAction::Pause) => {
            super::replication::pause(state, name, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Replication, JobAction::Resume) => {
            super::replication::resume(state, name, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Replication, JobAction::RunNow) => {
            // Background run — returns 202 + the (running) status immediately.
            let (code, resp) = super::replication::run_now(state, name, &headers).await?;
            Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)).into_response())
        }
        (JobSubsystem::Replication, JobAction::Verify) => {
            // Kicks off a BACKGROUND audit; returns 202 + the (running) status.
            let (code, resp) = super::replication::verify(
                state,
                name,
                &headers,
                super::replication::VerifyScope::All,
            )
            .await?;
            Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)).into_response())
        }
        (JobSubsystem::Lifecycle, JobAction::Pause) => {
            super::lifecycle::pause(state, name, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Lifecycle, JobAction::Resume) => {
            super::lifecycle::resume(state, name, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Lifecycle, JobAction::RunNow) => {
            // Background run — returns 202 + the run id immediately.
            let (code, resp) = super::lifecycle::run_now(state, name, &headers).await?;
            Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)).into_response())
        }
        (JobSubsystem::Lifecycle, JobAction::Preview) => {
            let resp = super::lifecycle::preview(state, name, q.fresh).await?;
            Ok((
                StatusCode::OK,
                Json(serde_json::to_value(resp).map_err(internal)?),
            )
                .into_response())
        }
        (JobSubsystem::Maintenance, JobAction::Cancel) => {
            let job_id: i64 = name.parse().map_err(|_| not_found())?;
            let Json(resp) =
                super::maintenance::cancel_job(State(state), Path(job_id), headers).await?;
            Ok((StatusCode::OK, Json(resp)).into_response())
        }
        (JobSubsystem::Maintenance, JobAction::Delete) => {
            let job_id: i64 = name.parse().map_err(|_| not_found())?;
            super::maintenance::delete_finished_job(&state, job_id, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Replication | JobSubsystem::Lifecycle, JobAction::Delete) => {
            delete_rule(&state, sub, &name, &headers).await?;
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        (JobSubsystem::Lifecycle, JobAction::Kill) => {
            let code = super::lifecycle::kill(state, name, &headers).await?;
            Ok((
                code,
                Json(serde_json::json!({"killing": true, "instance_local": true})),
            )
                .into_response())
        }
        (JobSubsystem::Replication, JobAction::Kill) => {
            let db = state.jobs_db.as_ref().ok_or_else(AdminError::no_jobs_db)?;
            let flipped = {
                let db = db.lock().await;
                db.replication_request_run_cancel(&name)?
            };
            if !flipped {
                return Err(AdminError::conflict("no running run to kill"));
            }
            crate::audit::audit_log("replication_kill", "admin", &name, &headers, "", "");
            // NOTE: no sync push — replication_run_history is NODE-LOCAL (not in
            // the IAM sync set), so a push cannot deliver the `cancelling` flip to
            // a peer. Kill reaches only the instance holding the lease; true
            // cross-instance kill is HA-roadmap work.
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"killing": true, "instance_local": true})),
            )
                .into_response())
        }
        _ => unreachable!("supported_actions gate covers the matrix"),
    }
}

/// Delete a replication/lifecycle rule: remove it from the YAML config,
/// rebuild the engine, persist, then purge its DB state/history/failure rows.
/// `reconcile_rules` is the existing orphan-prune — calling it with the
/// REMAINING rule names drops exactly the deleted rule's rows.
async fn delete_rule(
    state: &Arc<AdminState>,
    sub: JobSubsystem,
    name: &str,
    headers: &HeaderMap,
) -> Result<(), AdminError> {
    // Replication (H29): TAKE the rule's lease — the same coordination lease
    // the scheduler, run-now and the event consumer take (S3 when a
    // coordination bucket is configured, else SQLite) — and hold it through the
    // purge. A check-only `is_held` raced them: a run could take the lease
    // between the check and the purge. Taken BEFORE the config/db locks,
    // because the S3 lease does I/O we must not run under those locks.
    let delete_lease = if sub == JobSubsystem::Replication {
        match state.job_plane.as_ref() {
            Some(super::JobPlane { lease, .. }) => {
                let owner = format!("delete:{}", uuid::Uuid::new_v4());
                let now = crate::replication::state_store::current_unix_seconds();
                let ttl_secs = state
                    .config
                    .read()
                    .await
                    .jobs
                    .lease_timing(crate::config_sections::LeaseTiming::RULE_DELETE)
                    .ttl_secs;
                let held = crate::coordination::lease::try_acquire_one_shot(
                    &**lease,
                    crate::coordination::LeaseSubsystem::Replication,
                    name,
                    &owner,
                    now,
                    ttl_secs,
                )
                .await
                .map_err(|e| {
                    AdminError::unavailable(format!("could not take the rule lease: {e}"))
                })?;
                if !held {
                    return Err(AdminError::conflict(format!(
                        "rule '{name}' has a run or verify in progress — stop it before deleting"
                    )));
                }
                Some((lease.clone(), owner))
            }
            None => None,
        }
    } else {
        None
    };
    let result = delete_rule_locked(state, sub, name, headers, delete_lease.is_some()).await;
    if let Some((lease, owner)) = delete_lease {
        let _ = lease
            .release(
                crate::coordination::LeaseSubsystem::Replication,
                name,
                &owner,
            )
            .await;
    }
    result
}

/// The locked part of [`delete_rule`]. `holds_rule_lease`: the caller holds
/// the rule's coordination lease, so no run holds it (and the SQLite lease
/// check would see the caller's own lease).
///
/// ONE critical section (config.write OUTER → db.lock INNER, the codebase
/// order): liveness check, config retain + persist (the config write
/// pipeline, which also audits) and the DB row purge, all under the same
/// guards. For replication the caller's rule lease is the liveness anchor
/// (it exists before any run row); the run-row check covers a run whose
/// lease lapsed.
async fn delete_rule_locked(
    state: &Arc<AdminState>,
    sub: JobSubsystem,
    name: &str,
    headers: &HeaderMap,
    holds_rule_lease: bool,
) -> Result<(), AdminError> {
    use super::config::{HeldRefusal, Internal, InternalRefusal, OnPersistError};
    let hold = async {
        let db = match state.jobs_db.as_ref() {
            Some(db) => Some(db.lock().await),
            None => None,
        };
        (db, Vec::<String>::new())
    };
    let applied = super::config::run_internal_held(
        state,
        Internal {
            headers,
            action: match sub {
                JobSubsystem::Replication => "replication_delete",
                _ => "lifecycle_delete",
            },
            target: name,
            // A persist failure rolls back, so memory and disk never
            // diverge (a divergence would resurrect the rule on the next
            // restart).
            on_persist_error: OnPersistError::RollBack,
        },
        hold,
        |cfg, (db, remaining)| {
            // H2: refuse to delete a rule with a LIVE run. For replication: a
            // running/cancelling row OR an unexpired run/verify lease. For
            // lifecycle: an unexpired lifecycle lease (#10 — the guard was
            // replication-only, so a lifecycle rule could be purged mid-run).
            if let Some(db) = db.as_ref() {
                let now = crate::replication::state_store::current_unix_seconds();
                let blocked = match sub {
                    JobSubsystem::Replication => {
                        let run_status = db.replication_latest_run_status(name).ok().flatten();
                        let run_lease_live = !holds_rule_lease
                            && db.replication_lease_is_held(name, now).unwrap_or(false);
                        let lease_live =
                            run_lease_live || db.parity_lease_is_held(name, now).unwrap_or(false);
                        delete_blocked_by_live_run(run_status.as_deref(), lease_live)
                    }
                    JobSubsystem::Lifecycle => {
                        db.lifecycle_lease_is_held(name, now).unwrap_or(false)
                    }
                    JobSubsystem::Maintenance => false,
                };
                if blocked {
                    return Err(AdminError::conflict(format!(
                        "rule '{name}' has a run or verify in progress — stop it before deleting"
                    )));
                }
            }
            let (before, names): (usize, Vec<String>) = match sub {
                JobSubsystem::Replication => {
                    let before = cfg.replication.rules.len();
                    cfg.replication.rules.retain(|r| r.name != name);
                    let rules = &cfg.replication.rules;
                    (before, rules.iter().map(|r| r.name.clone()).collect())
                }
                JobSubsystem::Lifecycle => {
                    let before = cfg.lifecycle.rules.len();
                    cfg.lifecycle.rules.retain(|r| r.name != name);
                    let rules = &cfg.lifecycle.rules;
                    (before, rules.iter().map(|r| r.name.clone()).collect())
                }
                JobSubsystem::Maintenance => unreachable!(),
            };
            if names.len() == before {
                return Err(not_found());
            }
            *remaining = names;
            Ok(())
        },
        // Purge the deleted rule's DB rows under the SAME db guard as the
        // liveness check — no window for a run to start in between.
        |(db, remaining)| async move {
            if let Some(db) = db.as_ref() {
                let res = match sub {
                    JobSubsystem::Replication => db.replication_reconcile_rules(&remaining),
                    JobSubsystem::Lifecycle => db.lifecycle_reconcile_rules(&remaining),
                    JobSubsystem::Maintenance => unreachable!(),
                };
                if let Err(e) = res {
                    // Config is already deleted+persisted; orphan rows are
                    // harmless and get pruned on next boot. Log, don't fail.
                    tracing::warn!("rule '{name}' deleted but DB row purge failed: {e}");
                }
            }
            Ok(())
        },
    )
    .await;
    match applied {
        Ok(_) => Ok(()),
        Err(HeldRefusal::Edit(e)) => Err(e),
        Err(HeldRefusal::Persist { path, error }) => Err(AdminError::internal(format!(
            "rule delete FAILED to persist to {path} (rolled back, no change): {error}"
        ))),
        Err(HeldRefusal::Pipeline(InternalRefusal::Invalid { status, error })) => {
            Err(AdminError::status(status, error))
        }
        Err(HeldRefusal::Pipeline(r)) => Err(AdminError::internal(r.to_string())),
    }
}

/// Pure H2 liveness predicate: a rule is delete-blocked when its latest run
/// row is live OR any unexpired lease is held — the lease exists BEFORE the
/// run row, so checking the row alone races the acquire-then-spawn gap.
fn delete_blocked_by_live_run(latest_run_status: Option<&str>, lease_held: bool) -> bool {
    lease_held || matches!(latest_run_status, Some("running") | Some("cancelling"))
}

fn not_found() -> AdminError {
    AdminError::not_found("job not found")
}
fn internal<E: std::fmt::Display>(e: E) -> AdminError {
    AdminError::internal(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_job_id_table() {
        assert_eq!(
            parse_job_id("replication:nightly"),
            Some((JobSubsystem::Replication, "nightly"))
        );
        assert_eq!(
            parse_job_id("lifecycle:expire-old"),
            Some((JobSubsystem::Lifecycle, "expire-old"))
        );
        assert_eq!(
            parse_job_id("maintenance:42"),
            Some((JobSubsystem::Maintenance, "42"))
        );
        assert_eq!(parse_job_id("maintenance:abc"), None, "non-numeric key");
        assert_eq!(parse_job_id("replication:"), None, "empty key");
        assert_eq!(parse_job_id("unknown:x"), None);
        assert_eq!(parse_job_id("noseparator"), None);
        // Rule names may themselves contain ':'? They can't (config charset
        // is [A-Za-z0-9_.-]) — but split_once keeps the remainder intact
        // anyway, so a hypothetical 'a:b' rule still round-trips.
        assert_eq!(
            parse_job_id("replication:a:b"),
            Some((JobSubsystem::Replication, "a:b"))
        );
    }

    #[test]
    fn delete_liveness_predicate() {
        // The lease alone blocks (the acquire-to-run-row gap)...
        assert!(delete_blocked_by_live_run(None, true));
        assert!(delete_blocked_by_live_run(Some("succeeded"), true));
        // ...as does a live run row without a visible lease.
        assert!(delete_blocked_by_live_run(Some("running"), false));
        assert!(delete_blocked_by_live_run(Some("cancelling"), false));
        // Settled/no history + no lease = deletable.
        assert!(!delete_blocked_by_live_run(None, false));
        assert!(!delete_blocked_by_live_run(Some("succeeded"), false));
        assert!(!delete_blocked_by_live_run(Some("failed"), false));
        assert!(!delete_blocked_by_live_run(Some("cancelled"), false));
    }

    #[test]
    fn normalize_status_table() {
        for (raw, want) in [
            ("idle", "idle"),
            ("queued", "queued"),
            ("running", "running"),
            ("cancelling", "cancelling"),
            ("succeeded", "succeeded"),
            ("completed", "succeeded"),
            ("failed", "failed"),
            ("cancelled", "cancelled"),
            // A pause stop keeps its cursor: it is not a kill.
            ("paused", "paused"),
            // The proxy stopped it (a gate, a page budget): no operator did.
            ("deferred", "deferred"),
            ("stopped", "deferred"),
            ("something-new", "idle"),
            ("", "idle"),
        ] {
            assert_eq!(normalize_status(raw), want, "raw={raw}");
        }
    }

    #[test]
    fn lifecycle_progress_splits_the_skipped_objects() {
        let mut run = crate::lifecycle::LifecycleRunRecord {
            id: 7,
            rule_name: "purge".into(),
            triggered_by: "run-now".into(),
            started_at: 1,
            finished_at: Some(2),
            objects_scanned: 15,
            objects_affected: 4,
            objects_skipped: 11,
            bytes_affected: 100,
            errors: 0,
            status: "completed".into(),
            split: Some(crate::lifecycle::state_store::LifecycleRunSplit {
                objects_deleted: 4,
                objects_excluded: 9,
                objects_not_due: 2,
                ..Default::default()
            }),
        };
        let p = lifecycle_record_progress(&run);
        assert_eq!(
            (p.skipped, p.excluded, p.not_due, p.deleted),
            (11, Some(9), Some(2), Some(4))
        );
        // An older run without the split: unknown, not zero.
        run.split = None;
        let p = lifecycle_record_progress(&run);
        assert_eq!((p.excluded, p.not_due, p.deleted), (None, None, Some(0)));
    }

    /// A replication rule's row carries the live counters of its newest run
    /// (copied, in sync, failed, logical bytes, deleted) — not zeros.
    #[test]
    fn replication_row_progress_reads_the_newest_run() {
        assert_eq!(replication_row_progress(None), JobProgress::default());
        let run = crate::replication::state_store::RunRecord {
            id: 3,
            rule_name: "r".into(),
            triggered_by: "run-now".into(),
            started_at: 10,
            finished_at: None,
            objects_scanned: 9,
            objects_copied: 5,
            objects_skipped: 3,
            objects_deleted: 2,
            bytes_copied: 4096,
            errors: 1,
            status: "running".into(),
            delta_passthrough: 0,
            bytes_egress_saved: 0,
            reconstructed: 0,
        };
        assert_eq!(
            replication_row_progress(Some(&run)),
            JobProgress {
                processed: 5,
                total: None,
                bytes: 4096,
                failed: 1,
                skipped: 3,
                deleted: Some(2),
                excluded: None,
                not_due: None,
            }
        );
    }

    #[test]
    fn action_matrix() {
        use JobAction::*;
        assert_eq!(
            supported_actions(JobSubsystem::Replication),
            &[Pause, Resume, RunNow, Verify, Delete, Kill]
        );
        assert_eq!(
            supported_actions(JobSubsystem::Lifecycle),
            &[Pause, Resume, RunNow, Preview, Delete, Kill]
        );
        assert_eq!(
            supported_actions(JobSubsystem::Maintenance),
            &[Cancel, Delete]
        );
        assert_eq!(JobAction::parse("run-now"), Some(RunNow));
        assert_eq!(JobAction::parse("verify"), Some(Verify));
        assert_eq!(JobAction::parse("delete"), Some(Delete));
        assert_eq!(JobAction::parse("nope"), None);
        // Verify is replication-only.
        assert!(!supported_actions(JobSubsystem::Lifecycle).contains(&Verify));
    }

    fn oneoff(status: &str, phase: &str) -> MaintenanceJob {
        MaintenanceJob {
            id: 7,
            kind: "backfill-metadata".into(),
            bucket: "releases".into(),
            status: status.into(),
            phase: phase.into(),
            objects_total: Some(7),
            objects_done: 4,
            objects_skipped: 2,
            objects_failed: 1,
            bytes_done: 99,
            continuation_token: None,
            last_error: None,
            triggered_by: None,
            params: None,
            created_at: 1,
            started_at: Some(2),
            finished_at: Some(3),
            updated_at: 3,
        }
    }

    /// Overview B5: a backfill with 1 failure of 7 read "6 scanned, 6
    /// copied, 1 error". Scanned counts the failed object too.
    #[test]
    fn a_oneoff_run_entry_counts_failed_objects_as_scanned() {
        let e = maintenance_run_entry(&oneoff("completed_with_errors", "objects"));
        assert_eq!(e.objects_scanned, 7);
        assert_eq!(e.objects_processed, 4);
        assert_eq!(e.objects_skipped, 2);
        assert_eq!(e.errors, 1);
        assert_eq!(e.status, "completed_with_errors");
    }

    /// Maint M1: a finished migrate read "Phase: cleanup".
    #[test]
    fn a_completed_oneoff_reports_no_phase() {
        assert_eq!(maintenance_phase(&oneoff("completed", "cleanup")), None);
        assert_eq!(
            maintenance_phase(&oneoff("completed_with_errors", "cleanup")),
            None
        );
        assert_eq!(
            maintenance_phase(&oneoff("failed", "copy")).as_deref(),
            Some("copy"),
            "a failure keeps the phase it stopped in"
        );
        assert_eq!(
            maintenance_phase(&oneoff("running", "verify")).as_deref(),
            Some("verify")
        );
    }
}
