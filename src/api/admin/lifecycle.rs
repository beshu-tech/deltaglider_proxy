//! Admin API endpoints for delete-only lifecycle rules.

use super::AdminState;
use crate::lifecycle;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::info;

#[derive(Debug, Serialize)]
pub struct LifecycleOverview {
    pub worker_enabled: bool,
    pub tick_interval: String,
    pub rules: Vec<LifecycleRuleOverview>,
}

#[derive(Debug, Serialize)]
pub struct LifecycleRuleOverview {
    pub name: String,
    pub enabled: bool,
    pub bucket: String,
    pub prefix: String,
    pub action: String,
    pub expire_after: String,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub last_status: String,
    pub last_run_at: Option<i64>,
    pub next_due_at: i64,
    pub objects_affected_lifetime: i64,
    pub bytes_affected_lifetime: i64,
}

pub async fn list_rules(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<LifecycleOverview>, StatusCode> {
    let cfg = state.config.read().await;
    let lifecycle_cfg = &cfg.lifecycle;
    let db_guard = match state.config_db.as_ref() {
        Some(db) => Some(db.lock().await),
        None => None,
    };
    let now = lifecycle::current_unix_seconds();

    let mut rules = Vec::with_capacity(lifecycle_cfg.rules.len());
    for rule in &lifecycle_cfg.rules {
        let state_row = if let Some(db) = db_guard.as_ref() {
            let _ = db.lifecycle_ensure_state(&rule.name, now);
            db.lifecycle_load_state(&rule.name).ok().flatten()
        } else {
            None
        };
        rules.push(LifecycleRuleOverview {
            name: rule.name.clone(),
            enabled: rule.enabled,
            bucket: rule.bucket.clone(),
            prefix: rule.prefix.clone(),
            action: rule.action.kind().to_string(),
            expire_after: rule.expire_after.clone(),
            include_globs: rule.include_globs.clone(),
            exclude_globs: rule.exclude_globs.clone(),
            last_status: state_row
                .as_ref()
                .map(|s| s.last_status.clone())
                .unwrap_or_else(|| "idle".to_string()),
            last_run_at: state_row.as_ref().and_then(|s| s.last_run_at),
            next_due_at: state_row.as_ref().map(|s| s.next_due_at).unwrap_or(0),
            objects_affected_lifetime: state_row
                .as_ref()
                .map(|s| s.objects_affected_lifetime)
                .unwrap_or(0),
            bytes_affected_lifetime: state_row
                .as_ref()
                .map(|s| s.bytes_affected_lifetime)
                .unwrap_or(0),
        });
    }

    Ok(Json(LifecycleOverview {
        worker_enabled: lifecycle_cfg.enabled,
        tick_interval: lifecycle_cfg.tick_interval.clone(),
        rules,
    }))
}

pub async fn preview(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<lifecycle::LifecycleRunOutcome>, (StatusCode, String)> {
    let lifecycle_cfg = { state.config.read().await.lifecycle.clone() };
    let rule = lifecycle_cfg
        .rules
        .iter()
        .find(|rule| rule.name == name)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "rule not found".to_string()))?;

    let engine = state.s3_state.engine.load().clone();
    lifecycle::preview_rule(&engine, &rule, lifecycle_cfg.max_failures_retained as usize)
        .await
        .map(Json)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))
}

pub async fn run_now(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<lifecycle::LifecycleRunOutcome>, (StatusCode, String)> {
    let lifecycle_cfg = { state.config.read().await.lifecycle.clone() };
    let rule = lifecycle_cfg
        .rules
        .iter()
        .find(|rule| rule.name == name)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "rule not found".to_string()))?;

    if !lifecycle_cfg.enabled {
        return Err((
            StatusCode::CONFLICT,
            "lifecycle is globally disabled (storage.lifecycle.enabled = false)".to_string(),
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

    let Some(_guard) = lifecycle::try_acquire_rule(&rule.name) else {
        return Err((
            StatusCode::CONFLICT,
            "rule is already running; wait for the current run to finish".to_string(),
        ));
    };

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
    let lease_owner = format!("run-now:{}", uuid::Uuid::new_v4());
    let now = lifecycle::current_unix_seconds();
    {
        let db = db_arc.lock().await;
        db.lifecycle_ensure_state(&rule.name, now)
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        let acquired = db
            .lifecycle_try_acquire_lease(
                &rule.name,
                &lease_owner,
                now,
                lifecycle::scheduler::lease_ttl_secs(),
            )
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        if !acquired {
            return Err((
                StatusCode::CONFLICT,
                "rule is already running; wait for the current run to finish".to_string(),
            ));
        }
    }

    info!("Lifecycle run-now via admin API: rule='{}'", name);
    let engine = state.s3_state.engine.load().clone();
    let outcome = lifecycle::run_rule(
        Some(db_arc.clone()),
        &engine,
        &rule,
        lifecycle_cfg.max_failures_retained,
        "run-now",
        lifecycle::scheduler::scheduler_tick(&lifecycle_cfg).as_secs() as i64,
        Some(lifecycle::RunLease {
            owner: lease_owner.clone(),
            ttl_secs: lifecycle::scheduler::lease_ttl_secs(),
            heartbeat_secs: lifecycle::scheduler::heartbeat_secs(),
        }),
    )
    .await
    .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err));
    {
        let db = db_arc.lock().await;
        let _ = db.lifecycle_release_lease(&rule.name, &lease_owner);
    }
    let outcome = outcome?;

    crate::audit::audit_log(
        "lifecycle_run_now",
        "admin",
        &name,
        &HeaderMap::new(),
        &rule.bucket,
        &rule.prefix,
    );

    Ok(Json(outcome))
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub runs: Vec<HistoryEntry>,
}

#[derive(Debug, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub triggered_by: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub objects_scanned: i64,
    pub objects_affected: i64,
    pub objects_skipped: i64,
    pub bytes_affected: i64,
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
        .lifecycle_recent_runs(&name, limit)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(HistoryResponse {
        runs: runs
            .into_iter()
            .map(|run| HistoryEntry {
                id: run.id,
                triggered_by: run.triggered_by,
                started_at: run.started_at,
                finished_at: run.finished_at,
                objects_scanned: run.objects_scanned,
                objects_affected: run.objects_affected,
                objects_skipped: run.objects_skipped,
                bytes_affected: run.bytes_affected,
                errors: run.errors,
                status: run.status,
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
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub bucket: String,
    pub object_key: String,
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
        .lifecycle_recent_failures(&name, limit)
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(Json(FailuresResponse {
        failures: failures
            .into_iter()
            .map(|failure| FailureEntry {
                id: failure.id,
                run_id: failure.run_id,
                occurred_at: failure.occurred_at,
                bucket: failure.bucket,
                object_key: failure.object_key,
                error_message: failure.error_message,
            })
            .collect(),
    }))
}
