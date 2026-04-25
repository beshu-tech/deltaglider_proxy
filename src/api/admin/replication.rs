//! Admin API endpoints for lazy bucket replication.
//!
//! Routes (all session-gated via the surrounding middleware):
//!
//! - `GET  /_/api/admin/replication` — rules + state overview
//! - `POST /_/api/admin/replication/rules/:name/run-now`
//! - `POST /_/api/admin/replication/rules/:name/pause`
//! - `POST /_/api/admin/replication/rules/:name/resume`
//! - `GET  /_/api/admin/replication/rules/:name/history?limit=N`
//! - `GET  /_/api/admin/replication/rules/:name/failures?limit=N`

use super::AdminState;
use crate::replication;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

/// Response shape for `GET /_/api/admin/replication`.
#[derive(Debug, Serialize)]
pub struct ReplicationOverview {
    pub worker_enabled: bool,
    pub tick_interval: String,
    pub rules: Vec<RuleOverview>,
}

#[derive(Debug, Serialize)]
pub struct RuleOverview {
    pub name: String,
    pub enabled: bool,
    pub paused: bool,
    pub interval: String,
    pub source_bucket: String,
    pub source_prefix: String,
    pub destination_bucket: String,
    pub destination_prefix: String,
    pub last_status: String,
    pub last_run_at: Option<i64>,
    pub next_due_at: i64,
    pub objects_copied_lifetime: i64,
    pub bytes_copied_lifetime: i64,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<u32>,
}

pub async fn list_rules(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<ReplicationOverview>, StatusCode> {
    let cfg = state.config.read().await;
    let repl = &cfg.replication;

    let db = match state.config_db.as_ref() {
        Some(db) => db.lock().await,
        None => {
            return Ok(Json(ReplicationOverview {
                worker_enabled: repl.enabled,
                tick_interval: repl.tick_interval.clone(),
                rules: Vec::new(),
            }))
        }
    };

    let mut rules_out = Vec::with_capacity(repl.rules.len());
    for rule in &repl.rules {
        // Ensure state row exists so the overview can report sensible
        // defaults for never-run rules.
        let _ = db.replication_ensure_state(&rule.name, replication::current_unix_seconds());
        let st = db.replication_load_state(&rule.name).ok().flatten();
        rules_out.push(RuleOverview {
            name: rule.name.clone(),
            enabled: rule.enabled,
            paused: st.as_ref().map(|s| s.paused).unwrap_or(false),
            interval: rule.interval.clone(),
            source_bucket: rule.source.bucket.clone(),
            source_prefix: rule.source.prefix.clone(),
            destination_bucket: rule.destination.bucket.clone(),
            destination_prefix: rule.destination.prefix.clone(),
            last_status: st
                .as_ref()
                .map(|s| s.last_status.clone())
                .unwrap_or_else(|| "idle".to_string()),
            last_run_at: st.as_ref().and_then(|s| s.last_run_at),
            next_due_at: st.as_ref().map(|s| s.next_due_at).unwrap_or(0),
            objects_copied_lifetime: st.as_ref().map(|s| s.objects_copied_lifetime).unwrap_or(0),
            bytes_copied_lifetime: st.as_ref().map(|s| s.bytes_copied_lifetime).unwrap_or(0),
        });
    }

    Ok(Json(ReplicationOverview {
        worker_enabled: repl.enabled,
        tick_interval: repl.tick_interval.clone(),
        rules: rules_out,
    }))
}

/// Response for run-now.
#[derive(Debug, Serialize)]
pub struct RunNowResponse {
    pub run_id: i64,
    pub status: String,
    pub objects_scanned: i64,
    pub objects_copied: i64,
    pub objects_skipped: i64,
    pub bytes_copied: i64,
    pub errors: i64,
}

/// Trigger an immediate synchronous run of a rule. Used by the admin
/// UI + integration tests. Honours the paused flag: a paused rule
/// returns 409 Conflict.
pub async fn run_now(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<RunNowResponse>, (StatusCode, String)> {
    // Snapshot config first, release the lock immediately so we don't
    // hold it across the (potentially long) replication run.
    let repl = {
        let cfg = state.config.read().await;
        cfg.replication.clone()
    };
    let rule = repl
        .rules
        .iter()
        .find(|r| r.name == name)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "rule not found".to_string()))?;

    // M2 fix: respect both the global kill-switch (`replication.enabled`)
    // and the per-rule `enabled` flag. Pre-fix, an admin-triggered
    // `run-now` would copy objects even with `enabled=false` — making
    // the flag misleading documentation rather than an actual gate.
    if !repl.enabled {
        return Err((
            StatusCode::CONFLICT,
            "replication is globally disabled (storage.replication.enabled = false)".to_string(),
        ));
    }
    if !rule.enabled {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "rule '{}' is disabled (set enabled: true in YAML to run it)",
                rule.name
            ),
        ));
    }

    let db_arc = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .clone();

    // Short lock for the precheck only — run_rule acquires the lock
    // itself at each sync boundary (see its doc comment).
    {
        let db = db_arc.lock().await;
        let _ = db.replication_ensure_state(&rule.name, replication::current_unix_seconds());
        if let Ok(Some(st)) = db.replication_load_state(&rule.name) {
            if st.paused {
                return Err((
                    StatusCode::CONFLICT,
                    "rule is paused; resume it before running".to_string(),
                ));
            }
        }
    }

    info!("Replication run-now via admin API: rule='{}'", name);

    let engine = state.s3_state.engine.load().clone();
    let (run_id, outcome) =
        replication::run_rule(db_arc, &engine, &rule, repl.max_failures_retained)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

    crate::audit::audit_log(
        "replication_run_now",
        "admin",
        &name,
        &HeaderMap::new(),
        &rule.source.bucket,
        &rule.source.prefix,
    );

    Ok(Json(RunNowResponse {
        run_id,
        status: outcome.status,
        objects_scanned: outcome.totals.objects_scanned,
        objects_copied: outcome.totals.objects_copied,
        objects_skipped: outcome.totals.objects_skipped,
        bytes_copied: outcome.totals.bytes_copied,
        errors: outcome.totals.errors,
    }))
}

/// Check whether a rule with the given name exists in the live config.
/// M1 fix: previously pause/resume called `replication_ensure_state`
/// before this check, leaving an orphan DB row for ghost rules even
/// though the response was 404. This snapshot-and-find is now the
/// FIRST thing pause/resume do.
async fn rule_in_config(state: &AdminState, name: &str) -> bool {
    let cfg = state.config.read().await;
    cfg.replication.rules.iter().any(|r| r.name == name)
}

pub async fn pause(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !rule_in_config(&state, &name).await {
        return Err((StatusCode::NOT_FOUND, "rule not found".to_string()));
    }
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;
    let _ = db.replication_ensure_state(&name, replication::current_unix_seconds());
    db.replication_set_paused(&name, true)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    crate::audit::audit_log(
        "replication_pause",
        "admin",
        &name,
        &HeaderMap::new(),
        "",
        "",
    );
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !rule_in_config(&state, &name).await {
        return Err((StatusCode::NOT_FOUND, "rule not found".to_string()));
    }
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;
    let _ = db.replication_ensure_state(&name, replication::current_unix_seconds());
    db.replication_set_paused(&name, false)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    crate::audit::audit_log(
        "replication_resume",
        "admin",
        &name,
        &HeaderMap::new(),
        "",
        "",
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub runs: Vec<HistoryEntry>,
}

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub objects_scanned: i64,
    pub objects_copied: i64,
    pub objects_skipped: i64,
    pub objects_deleted: i64,
    pub bytes_copied: i64,
    pub errors: i64,
    pub status: String,
}

pub async fn history(
    Path(name): Path<String>,
    Query(q): Query<LimitQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<HistoryResponse>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;
    let runs = db
        .replication_recent_runs(&name, limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(HistoryResponse {
        runs: runs
            .into_iter()
            .map(|r| HistoryEntry {
                id: r.id,
                started_at: r.started_at,
                finished_at: r.finished_at,
                objects_scanned: r.objects_scanned,
                objects_copied: r.objects_copied,
                objects_skipped: r.objects_skipped,
                objects_deleted: r.objects_deleted,
                bytes_copied: r.bytes_copied,
                errors: r.errors,
                status: r.status,
            })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct FailuresResponse {
    pub failures: Vec<FailureEntry>,
}

#[derive(Debug, Serialize)]
pub struct FailureEntry {
    pub id: i64,
    pub occurred_at: i64,
    pub source_key: String,
    pub dest_key: String,
    pub error_message: String,
}

pub async fn failures(
    Path(name): Path<String>,
    Query(q): Query<LimitQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<FailuresResponse>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;
    let failures = db
        .replication_recent_failures(&name, limit)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    Ok(Json(FailuresResponse {
        failures: failures
            .into_iter()
            .map(|f| FailureEntry {
                id: f.id,
                occurred_at: f.occurred_at,
                source_key: f.source_key,
                dest_key: f.dest_key,
                error_message: f.error_message,
            })
            .collect(),
    }))
}
