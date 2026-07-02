// SPDX-License-Identifier: GPL-3.0-only

//! Admin API endpoints for delete-only lifecycle rules.

use super::AdminState;
use crate::lifecycle;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use std::sync::Arc;
use tracing::info;

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

    // Symmetry with run_now: a fatal-config rule 400s up front rather than
    // surfacing the same error deeper in preview_rule.
    let fatal = lifecycle::planner::lifecycle_rule_errors(&rule);
    if !fatal.is_empty() {
        let msg = fatal.join("; ");
        return Err((lifecycle::classify_lifecycle_run_error(&msg), msg));
    }
    // Duplicate-name defence, mirroring the scheduler: same-named rules share
    // one name-keyed state row — running "the first match" would corrupt the
    // other rule's cursor/lease.
    if lifecycle::planner::duplicate_rule_names(lifecycle_cfg.rules.iter()).contains(&rule.name) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "rule name '{}' is duplicated in the config — rename one of the copies first",
                rule.name
            ),
        ));
    }

    let engine = state.s3_state.engine.load().clone();
    lifecycle::preview_rule(&engine, &rule, lifecycle_cfg.max_failures_retained as usize)
        .await
        .map(Json)
        .map_err(|err| (lifecycle::classify_lifecycle_run_error(&err), err))
}

/// Pause a lifecycle rule (scheduler skips it; run-now 409s).
pub async fn pause(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_paused(&state, &name, true, "lifecycle_pause").await
}

/// Resume a paused lifecycle rule.
pub async fn resume(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<StatusCode, (StatusCode, String)> {
    set_paused(&state, &name, false, "lifecycle_resume").await
}

async fn set_paused(
    state: &Arc<AdminState>,
    name: &str,
    paused: bool,
    audit_action: &str,
) -> Result<StatusCode, (StatusCode, String)> {
    let in_config = {
        let cfg = state.config.read().await;
        cfg.lifecycle.rules.iter().any(|r| r.name == name)
    };
    if !in_config {
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
    let _ = db.lifecycle_ensure_state(name, lifecycle::current_unix_seconds());
    db.lifecycle_set_paused(name, paused)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
    crate::audit::audit_log(audit_action, "admin", name, &HeaderMap::new(), "", "");
    Ok(StatusCode::NO_CONTENT)
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

    // Pre-gate: a fatal-config rule (missing expire_after, empty transition
    // dest, …) must 400 BEFORE we take a lease / open a run — otherwise the
    // worker errors mid-run and records a spurious FAILED run row (the exact
    // recurring-FAILED noise the config gate + scheduler-skip exist to stop).
    let fatal = lifecycle::planner::lifecycle_rule_errors(&rule);
    if !fatal.is_empty() {
        let msg = fatal.join("; ");
        return Err((lifecycle::classify_lifecycle_run_error(&msg), msg));
    }
    // Duplicate-name defence, mirroring the scheduler: same-named rules share
    // one name-keyed state row — running "the first match" would corrupt the
    // other rule's cursor/lease.
    if lifecycle::planner::duplicate_rule_names(lifecycle_cfg.rules.iter()).contains(&rule.name) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "rule name '{}' is duplicated in the config — rename one of the copies first",
                rule.name
            ),
        ));
    }

    // Same deferral the scheduler applies: run-now must not write into a
    // bucket a maintenance job (re-encrypt / migrate) is rewriting.
    if let Some(busy) = lifecycle::planner::rule_write_buckets(&rule)
        .into_iter()
        .find(|b| state.s3_state.maintenance_gate.is_busy(b))
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "bucket '{busy}' has an active maintenance job — run the rule again \
                 when it finishes"
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
        // Paused beats run-now (same contract as replication): the operator
        // paused the rule for a reason; an explicit run must not sidestep it.
        let paused = db
            .lifecycle_load_state(&rule.name)
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?
            .map(|st| st.paused)
            .unwrap_or(false);
        if paused {
            return Err((
                StatusCode::CONFLICT,
                format!("rule '{}' is paused — resume it first", rule.name),
            ));
        }
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
        Some(state.s3_state.maintenance_gate.clone()),
    )
    .await
    .map_err(|err| (lifecycle::classify_lifecycle_run_error(&err), err));
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
