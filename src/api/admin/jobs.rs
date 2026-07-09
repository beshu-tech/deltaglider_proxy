// SPDX-License-Identifier: GPL-3.0-only

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

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::AdminState;
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
/// `idle | queued | running | cancelling | succeeded | completed_with_errors |
/// failed | cancelled`.
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
        // A run halted mid-sweep by an operator pause is terminal-but-interrupted —
        // surface it as `cancelled` (same vocabulary as an explicit cancel) so the
        // UI stops showing it as `running`. Cursor is preserved; resume continues.
        "cancelled" | "stopped" => "cancelled",
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
        ],
        JobSubsystem::Maintenance => &[JobAction::Cancel],
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

#[derive(Debug, Serialize)]
pub struct JobProgress {
    pub processed: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    pub bytes: i64,
    pub failed: i64,
    pub skipped: i64,
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
    pub detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct JobsOverview {
    pub workers: serde_json::Value,
    pub jobs: Vec<JobView>,
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
}

fn maintenance_job_view(j: &MaintenanceJob) -> JobView {
    let percent = crate::maintenance::display_percent(j);
    let (target, detail) = match (j.kind.as_str(), j.params.as_deref()) {
        ("migrate", Some(p)) => match migrate::parse_params(p) {
            Ok(mp) => (
                Some(mp.target_backend.clone()),
                serde_json::json!({
                    "target_backend": mp.target_backend,
                    "from_backend": mp.from_backend,
                    "delete_source": mp.delete_source,
                }),
            ),
            Err(_) => (None, serde_json::json!({})),
        },
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
        phase: Some(j.phase.clone()),
        percent,
        progress: JobProgress {
            processed: j.objects_done,
            total: j.objects_total,
            bytes: j.bytes_done,
            failed: j.objects_failed,
            skipped: j.objects_skipped,
        },
        lifetime: None,
        last_run_at: None,
        next_due_at: None,
        created_at: Some(j.created_at),
        started_at: j.started_at,
        finished_at: j.finished_at,
        last_error: j.last_error.clone(),
        detail,
    }
}

// ─────────────────────────── handlers ───────────────────────────

/// GET /_/api/admin/jobs
pub async fn list_jobs(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<JobsOverview>, (StatusCode, String)> {
    let cfg = state.config.read().await;
    let repl_cfg = cfg.replication.clone();
    let lc_cfg = cfg.lifecycle.clone();
    drop(cfg);

    let mut jobs: Vec<JobView> = Vec::new();
    let mut last_event_applied_at: Option<i64> = None;

    if let Some(db) = state.config_db.as_ref() {
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
            if status_raw == "running" {
                if let Ok(Some(live)) = db.replication_latest_run_status(&rule.name) {
                    if live == "cancelling" {
                        status_raw = live;
                    }
                }
            }
            jobs.push(JobView {
                id: format!("replication:{}", rule.name),
                kind: "replication".into(),
                name: rule.name.clone(),
                scope: JobScope {
                    bucket: rule.source.bucket.clone(),
                    prefix: (!rule.source.prefix.is_empty()).then(|| rule.source.prefix.clone()),
                    target: Some(rule.destination.bucket.clone()),
                },
                trigger: "continuous",
                enabled: Some(rule.enabled),
                paused: Some(st.as_ref().map(|s| s.paused).unwrap_or(false)),
                status: normalize_status(&status_raw),
                status_raw,
                phase: None,
                percent: None,
                progress: JobProgress {
                    processed: 0,
                    total: None,
                    bytes: 0,
                    failed: 0,
                    skipped: 0,
                },
                lifetime: Some(JobLifetime {
                    objects: st.as_ref().map(|s| s.objects_copied_lifetime).unwrap_or(0),
                    bytes: st.as_ref().map(|s| s.bytes_copied_lifetime).unwrap_or(0),
                }),
                last_run_at: st.as_ref().and_then(|s| s.last_run_at),
                next_due_at: st.as_ref().map(|s| s.next_due_at),
                created_at: None,
                started_at: None,
                finished_at: None,
                last_error: None,
                detail: serde_json::json!({
                    "interval": rule.interval,
                    "destination_prefix": rule.destination.prefix,
                    // Live "currently copying" objects (largest first, top 3)
                    // so a slow-moving counter is explained in the UI — a
                    // 4 GB tarball at 10 MB/s is work, not a hang.
                    "in_flight": crate::replication::worker::inflight_snapshot(&rule.name)
                        .into_iter()
                        .take(3)
                        .collect::<Vec<_>>(),
                }),
            });
        }

        for rule in &lc_cfg.rules {
            let st = db.lifecycle_load_state(&rule.name).ok().flatten();
            let status_raw = st
                .as_ref()
                .map(|s| s.last_status.clone())
                .unwrap_or_else(|| "idle".to_string());
            jobs.push(JobView {
                id: format!("lifecycle:{}", rule.name),
                kind: "lifecycle".into(),
                name: rule.name.clone(),
                scope: JobScope {
                    bucket: rule.bucket.clone(),
                    prefix: (!rule.prefix.is_empty()).then(|| rule.prefix.clone()),
                    target: None,
                },
                trigger: "scheduled",
                enabled: Some(rule.enabled),
                paused: Some(st.as_ref().map(|s| s.paused).unwrap_or(false)),
                status: normalize_status(&status_raw),
                status_raw,
                phase: None,
                percent: None,
                progress: JobProgress {
                    processed: 0,
                    total: None,
                    bytes: 0,
                    failed: 0,
                    skipped: 0,
                },
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
                started_at: None,
                finished_at: None,
                last_error: None,
                detail: serde_json::json!({
                    "action": rule.action.kind(),
                    "expire_after": rule.expire_after,
                    "retain_count": match &rule.action {
                        crate::config_sections::LifecycleAction::RetainNewest(a) => Some(a.count),
                        _ => None,
                    },
                    "include_globs": rule.include_globs,
                    "exclude_globs": rule.exclude_globs,
                }),
            });
        }

        for j in db
            .maintenance_list_jobs(50)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            jobs.push(maintenance_job_view(&j));
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
    }))
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
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let Json(resp) = super::replication::verify_status(Path(key.to_string()), State(state)).await?;
    Ok(Json(serde_json::to_value(resp).map_err(internal)?))
}

/// POST /_/api/admin/jobs/:id/verify — kick off the background parity audit
/// (replication only). Returns 202 + the running status. The literal `verify`
/// route carries this POST because it shadows the `:action` param at this path.
pub async fn job_verify_start(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let (code, Json(resp)) =
        super::replication::verify(Path(key.to_string()), State(state)).await?;
    Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)))
}

/// POST /_/api/admin/jobs/:id/verify/cancel — cancel a running parity audit
/// (replication only).
pub async fn job_verify_cancel(
    Path(id): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    if sub != JobSubsystem::Replication {
        return Err(not_found());
    }
    let Json(resp) = super::replication::verify_cancel(Path(key.to_string()), State(state)).await?;
    Ok(Json(serde_json::to_value(resp).map_err(internal)?))
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
    Query(q): Query<LimitQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .config_db
        .as_ref()
        .ok_or(db_unavailable())?
        .lock()
        .await;
    let runs: Vec<JobRunEntry> = match sub {
        JobSubsystem::Replication => db
            .replication_recent_runs(key, limit)
            .map_err(internal)?
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
            })
            .collect(),
        JobSubsystem::Lifecycle => db
            .lifecycle_recent_runs(key, limit)
            .map_err(internal)?
            .into_iter()
            .map(|r| JobRunEntry {
                id: r.id,
                triggered_by: r.triggered_by,
                started_at: r.started_at,
                finished_at: r.finished_at,
                status: normalize_status(&r.status),
                status_raw: r.status,
                objects_scanned: r.objects_scanned,
                objects_processed: r.objects_affected,
                objects_skipped: r.objects_skipped,
                objects_deleted: None,
                bytes: r.bytes_affected,
                errors: r.errors,
                delta_passthrough: 0,
                bytes_egress_saved: 0,
            })
            .collect(),
        JobSubsystem::Maintenance => {
            let job_id: i64 = key.parse().map_err(|_| not_found())?;
            let job = db
                .maintenance_job_by_id(job_id)
                .map_err(internal)?
                .ok_or(not_found())?;
            vec![JobRunEntry {
                id: job.id,
                triggered_by: job.triggered_by.clone().unwrap_or_else(|| "admin".into()),
                started_at: job.started_at.unwrap_or(job.created_at),
                finished_at: job.finished_at,
                status: normalize_status(&job.status),
                status_raw: job.status.clone(),
                objects_scanned: job.objects_done + job.objects_skipped,
                objects_processed: job.objects_done,
                objects_skipped: job.objects_skipped,
                objects_deleted: None,
                bytes: job.bytes_done,
                errors: job.objects_failed,
                delta_passthrough: 0,
                bytes_egress_saved: 0,
            }]
        }
    };
    Ok(Json(serde_json::json!({ "runs": runs })))
}

/// GET /_/api/admin/jobs/:id/failures
pub async fn job_failures(
    Path(id): Path<String>,
    Query(q): Query<LimitQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .config_db
        .as_ref()
        .ok_or(db_unavailable())?
        .lock()
        .await;
    let failures: Vec<JobFailureEntry> = match sub {
        JobSubsystem::Replication => db
            .replication_recent_failures(key, limit)
            .map_err(internal)?
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
            })
            .collect(),
        JobSubsystem::Lifecycle => db
            .lifecycle_recent_failures(key, limit)
            .map_err(internal)?
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
            })
            .collect(),
        JobSubsystem::Maintenance => {
            let job_id: i64 = key.parse().map_err(|_| not_found())?;
            db.maintenance_list_failures(job_id, limit as usize)
                .map_err(internal)?
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
                })
                .collect()
        }
    };
    Ok(Json(serde_json::json!({ "failures": failures })))
}

/// POST /_/api/admin/jobs/:id/:action — uniform action dispatch.
pub async fn job_action(
    Path((id, action)): Path<(String, String)>,
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let (sub, key) = parse_job_id(&id).ok_or(not_found())?;
    let action = JobAction::parse(&action)
        .ok_or((StatusCode::NOT_FOUND, format!("unknown action '{action}'")))?;
    if !supported_actions(sub).contains(&action) {
        let available = supported_actions(sub)
            .iter()
            .map(|a| a.wire_name())
            .collect::<Vec<_>>()
            .join(", ");
        return Err((
            StatusCode::BAD_REQUEST,
            format!("this action isn't available for this job kind (available: {available})"),
        ));
    }
    let name = key.to_string();
    match (sub, action) {
        (JobSubsystem::Replication, JobAction::Pause) => {
            super::replication::pause(Path(name), State(state)).await?;
            Ok((StatusCode::NO_CONTENT, Json(serde_json::json!({}))))
        }
        (JobSubsystem::Replication, JobAction::Resume) => {
            super::replication::resume(Path(name), State(state)).await?;
            Ok((StatusCode::NO_CONTENT, Json(serde_json::json!({}))))
        }
        (JobSubsystem::Replication, JobAction::RunNow) => {
            // Background run — returns 202 + the (running) status immediately.
            let (code, Json(resp)) = super::replication::run_now(Path(name), State(state)).await?;
            Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)))
        }
        (JobSubsystem::Replication, JobAction::Verify) => {
            // Kicks off a BACKGROUND audit; returns 202 + the (running) status.
            let (code, Json(resp)) = super::replication::verify(Path(name), State(state)).await?;
            Ok((code, Json(serde_json::to_value(resp).map_err(internal)?)))
        }
        (JobSubsystem::Lifecycle, JobAction::Pause) => {
            super::lifecycle::pause(Path(name), State(state)).await?;
            Ok((StatusCode::NO_CONTENT, Json(serde_json::json!({}))))
        }
        (JobSubsystem::Lifecycle, JobAction::Resume) => {
            super::lifecycle::resume(Path(name), State(state)).await?;
            Ok((StatusCode::NO_CONTENT, Json(serde_json::json!({}))))
        }
        (JobSubsystem::Lifecycle, JobAction::RunNow) => {
            let Json(resp) = super::lifecycle::run_now(Path(name), State(state)).await?;
            Ok((
                StatusCode::OK,
                Json(serde_json::to_value(resp).map_err(internal)?),
            ))
        }
        (JobSubsystem::Lifecycle, JobAction::Preview) => {
            let Json(resp) = super::lifecycle::preview(Path(name), State(state)).await?;
            Ok((
                StatusCode::OK,
                Json(serde_json::to_value(resp).map_err(internal)?),
            ))
        }
        (JobSubsystem::Maintenance, JobAction::Cancel) => {
            let job_id: i64 = name.parse().map_err(|_| not_found())?;
            let Json(resp) =
                super::maintenance::cancel_job(State(state), Path(job_id), headers).await?;
            Ok((StatusCode::OK, Json(resp)))
        }
        (JobSubsystem::Replication | JobSubsystem::Lifecycle, JobAction::Delete) => {
            delete_rule(&state, sub, &name, &headers).await?;
            Ok((StatusCode::NO_CONTENT, Json(serde_json::json!({}))))
        }
        (JobSubsystem::Replication, JobAction::Kill) => {
            let db = state.config_db.as_ref().ok_or_else(db_unavailable)?;
            let flipped = {
                let db = db.lock().await;
                db.replication_request_run_cancel(&name).map_err(internal)?
            };
            if !flipped {
                return Err((StatusCode::CONFLICT, "no running run to kill".to_string()));
            }
            crate::audit::audit_log("replication_kill", "admin", &name, &headers, "", "");
            // NOTE: no sync push — replication_run_history is NODE-LOCAL (not in
            // the IAM sync set), so a push cannot deliver the `cancelling` flip to
            // a peer. Kill reaches only the instance holding the lease; true
            // cross-instance kill is HA-roadmap work.
            Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({"killing": true, "instance_local": true})),
            ))
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
) -> Result<(), (StatusCode, String)> {
    // ONE critical section (config.write OUTER → db.lock INNER, the codebase
    // order): liveness check, config retain+persist, and the DB row purge all
    // under the same guards. Holding the db lock across the purge closes the
    // H2 race — run-now/scheduler acquire the lease under this same lock, and
    // the LEASE (taken before the run row exists) is the liveness anchor.
    {
        let mut cfg = state.config.write().await;
        let db_guard = match state.config_db.as_ref() {
            Some(db) => Some(db.lock().await),
            None => None,
        };

        // H2: refuse to delete a rule with a LIVE run. For replication: a
        // running/cancelling row OR an unexpired run/verify lease. For
        // lifecycle: an unexpired lifecycle lease (#10 — the guard was
        // replication-only, so a lifecycle rule could be purged mid-run).
        if let Some(db) = db_guard.as_ref() {
            let now = crate::replication::state_store::current_unix_seconds();
            let blocked = match sub {
                JobSubsystem::Replication => {
                    let run_status = db.replication_latest_run_status(name).ok().flatten();
                    let lease_live = db.replication_lease_is_held(name, now).unwrap_or(false)
                        || db.parity_lease_is_held(name, now).unwrap_or(false);
                    delete_blocked_by_live_run(run_status.as_deref(), lease_live)
                }
                JobSubsystem::Lifecycle => db.lifecycle_lease_is_held(name, now).unwrap_or(false),
                JobSubsystem::Maintenance => false,
            };
            if blocked {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "rule '{name}' has a run or verify in progress — stop it before deleting"
                    ),
                ));
            }
        }

        // Remove from config + persist, restoring on persist failure so memory
        // and disk never diverge (a divergence would resurrect the rule on the
        // next restart). The engine does NOT read replication/lifecycle rules,
        // so no engine rebuild is needed.
        let rollback = cfg.clone();
        let existed = match sub {
            JobSubsystem::Replication => {
                let before = cfg.replication.rules.len();
                cfg.replication.rules.retain(|r| r.name != name);
                cfg.replication.rules.len() != before
            }
            JobSubsystem::Lifecycle => {
                let before = cfg.lifecycle.rules.len();
                cfg.lifecycle.rules.retain(|r| r.name != name);
                cfg.lifecycle.rules.len() != before
            }
            JobSubsystem::Maintenance => unreachable!(),
        };
        if !existed {
            return Err(not_found());
        }
        let path = super::config::active_config_path(state);
        if let Err(e) = cfg.persist_to_file(&path) {
            *cfg = rollback;
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("rule delete FAILED to persist to {path} (rolled back, no change): {e}"),
            ));
        }
        let remaining: Vec<String> = match sub {
            JobSubsystem::Replication => cfg
                .replication
                .rules
                .iter()
                .map(|r| r.name.clone())
                .collect(),
            JobSubsystem::Lifecycle => cfg.lifecycle.rules.iter().map(|r| r.name.clone()).collect(),
            JobSubsystem::Maintenance => unreachable!(),
        };

        // Purge the deleted rule's DB rows under the SAME db guard as the
        // liveness check — no window for a run to start in between.
        if let Some(db) = db_guard.as_ref() {
            let res = match sub {
                JobSubsystem::Replication => db.replication_reconcile_rules(&remaining),
                JobSubsystem::Lifecycle => db.lifecycle_reconcile_rules(&remaining),
                JobSubsystem::Maintenance => unreachable!(),
            };
            if let Err(e) = res {
                // Config is already deleted+persisted; orphan rows are harmless
                // and get pruned on next boot. Log, don't fail the delete.
                tracing::warn!("rule '{name}' deleted but DB row purge failed: {e}");
            }
        }
    }

    crate::audit::audit_log(
        match sub {
            JobSubsystem::Replication => "replication_delete",
            _ => "lifecycle_delete",
        },
        "admin",
        name,
        headers,
        "",
        "",
    );
    Ok(())
}

/// Pure H2 liveness predicate: a rule is delete-blocked when its latest run
/// row is live OR any unexpired lease is held — the lease exists BEFORE the
/// run row, so checking the row alone races the acquire-then-spawn gap.
fn delete_blocked_by_live_run(latest_run_status: Option<&str>, lease_held: bool) -> bool {
    lease_held || matches!(latest_run_status, Some("running") | Some("cancelling"))
}

fn not_found() -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, "job not found".to_string())
}
fn db_unavailable() -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "config DB not available".to_string(),
    )
}
fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
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
            ("something-new", "idle"),
            ("", "idle"),
        ] {
            assert_eq!(normalize_status(raw), want, "raw={raw}");
        }
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
            &[Pause, Resume, RunNow, Preview, Delete]
        );
        assert_eq!(supported_actions(JobSubsystem::Maintenance), &[Cancel]);
        assert_eq!(JobAction::parse("run-now"), Some(RunNow));
        assert_eq!(JobAction::parse("verify"), Some(Verify));
        assert_eq!(JobAction::parse("delete"), Some(Delete));
        assert_eq!(JobAction::parse("nope"), None);
        // Verify is replication-only.
        assert!(!supported_actions(JobSubsystem::Lifecycle).contains(&Verify));
    }
}
