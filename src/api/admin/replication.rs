// SPDX-License-Identifier: GPL-3.0-only

//! Per-rule replication actions (run-now / pause / resume), consumed by
//! the unified jobs API (`api/admin/jobs.rs`) under
//! `POST /_/api/admin/jobs/replication:<rule>/{run-now,pause,resume}`.
//! Listing, runs, and failures live in the jobs module.

use super::AdminState;
use crate::config_sections::{ReplicationConfig, ReplicationRule};
use crate::replication;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Serialize;
use std::sync::Arc;
use tracing::info;

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
/// Snapshot the replication config (lock released immediately) and find the
/// named rule, or 404. Returns the whole `repl` too — callers need its flags.
async fn snapshot_and_find_rule(
    state: &Arc<AdminState>,
    name: &str,
) -> Result<(ReplicationConfig, ReplicationRule), (StatusCode, String)> {
    let repl = { state.config.read().await.replication.clone() };
    let rule = repl
        .rules
        .iter()
        .find(|r| r.name == name)
        .cloned()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "rule not found".to_string()))?;
    Ok((repl, rule))
}

pub async fn run_now(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<RunNowResponse>, (StatusCode, String)> {
    let (repl, rule) = snapshot_and_find_rule(&state, &name).await?;

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

    // Same deferral the scheduler and event consumer apply: run-now must
    // not write into a destination a maintenance job is rewriting.
    if state
        .s3_state
        .maintenance_gate
        .is_busy(&rule.destination.bucket)
    {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "destination bucket '{}' has an active maintenance job — run the rule \
                 again when it finishes",
                rule.destination.bucket
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

    let lease_owner = format!("run-now:{}", uuid::Uuid::new_v4());

    // Short lock for the precheck + lease acquisition only — run_rule
    // acquires the lock itself at each sync boundary (see its doc comment).
    {
        let db = db_arc.lock().await;
        let now = replication::current_unix_seconds();
        let _ = db.replication_ensure_state(&rule.name, now);
        // Acquire the lease FIRST, then re-check `paused` while still holding
        // the same DB lock. The lease is the true serialization anchor: making
        // it the first mutation closes the check-then-act window where a
        // concurrent pause/resume could toggle the flag between a standalone
        // paused check and lease acquisition. Both the read and the lease grant
        // happen under one uninterrupted lock hold, so the decision is atomic.
        let acquired = db
            .replication_try_acquire_lease(
                &rule.name,
                &lease_owner,
                now,
                replication::scheduler::lease_ttl_secs(&repl),
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;
        if !acquired {
            return Err((
                StatusCode::CONFLICT,
                "rule is already running; wait for the current run to finish".to_string(),
            ));
        }
        // Paused check after we own the lease — if the rule is paused, release
        // the lease we just took so a later resume+run isn't blocked by a
        // dangling lease, and return 409.
        if let Ok(Some(st)) = db.replication_load_state(&rule.name) {
            if st.paused {
                let _ = db.replication_release_lease(&rule.name, &lease_owner);
                return Err((
                    StatusCode::CONFLICT,
                    "rule is paused; resume it before running".to_string(),
                ));
            }
        }
    }

    info!("Replication run-now via admin API: rule='{}'", name);

    let engine = state.s3_state.engine.load().clone();
    let run_result = replication::run_rule(
        db_arc.clone(),
        &engine,
        &rule,
        repl.max_failures_retained,
        replication::scheduler::object_timeout(&repl),
        repl.object_skip_after_failures,
        "run-now",
        Some(replication::RunLease {
            owner: lease_owner.clone(),
            ttl_secs: replication::scheduler::lease_ttl_secs(&repl),
            heartbeat_secs: replication::scheduler::heartbeat_secs(&repl),
        }),
        replication::RunConcurrency {
            transfers: repl.transfers,
            upload_concurrency: repl.upload_concurrency,
        },
    )
    .await;

    {
        let db = db_arc.lock().await;
        let _ = db.replication_release_lease(&rule.name, &lease_owner);
    }

    let (run_id, outcome) =
        run_result.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)))?;

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

/// The poll envelope for the parity audit (a background job). `status` is
/// idle | running | done | failed. `outcome` is the last completed verdict
/// (kept while a new scan runs). The frontend polls `GET verify`.
#[derive(serde::Serialize)]
pub struct ParityStatusResponse {
    pub status: String,
    pub progress_scanned: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scanned_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<replication::ParityOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn parity_status_from_row(
    row: Option<crate::replication::ParityResultRow>,
) -> ParityStatusResponse {
    let Some(row) = row else {
        return ParityStatusResponse {
            status: "idle".into(),
            progress_scanned: 0,
            scanned_at: None,
            outcome: None,
            error: None,
        };
    };
    let outcome = row
        .outcome_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    ParityStatusResponse {
        status: row.status,
        progress_scanned: row.progress_scanned,
        scanned_at: row.scanned_at,
        outcome,
        error: row.last_error,
    }
}

/// POST: kick off a parity audit as a BACKGROUND job and return immediately
/// (202). If an audit is already running, just report its status. The result
/// is persisted server-side so it survives navigation + restart; poll
/// `GET verify`. Gated only on rule existence (auditing a disabled rule is
/// valid). Idempotent under the lease — a second POST won't double-scan.
pub async fn verify(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<(StatusCode, Json<ParityStatusResponse>), (StatusCode, String)> {
    let (_repl, rule) = snapshot_and_find_rule(&state, &name).await?;
    let Some(db_arc) = state.config_db.clone() else {
        // No config DB → fall back to a synchronous in-request audit (dev/no-DB).
        let engine = state.s3_state.engine.load().clone();
        let outcome = replication::parity_audit(
            &engine,
            &rule,
            replication::parity::MAX_PARITY_OBJECTS,
            None,
            None,
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        return Ok((
            StatusCode::OK,
            Json(ParityStatusResponse {
                status: "done".into(),
                progress_scanned: outcome.source_objects as i64,
                scanned_at: Some(outcome.scanned_at),
                outcome: Some(outcome),
                error: None,
            }),
        ));
    };

    let owner = format!("verify:{}", uuid::Uuid::new_v4());
    // Acquire the lease; if someone else holds it, just report current status.
    let now = crate::replication::current_unix_seconds();
    let acquired = {
        let db = db_arc.lock().await;
        db.parity_try_acquire_lease(&rule.name, &owner, now, PARITY_LEASE_TTL_SECS)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    };
    if !acquired {
        // Someone else holds the lease → a scan IS in flight. Report 'running'
        // even if the winner hasn't written its set_running row yet (a small
        // race window) — otherwise the loser would see 'idle' and never poll.
        let row = {
            let db = db_arc.lock().await;
            db.parity_result_load(&rule.name).ok().flatten()
        };
        let mut resp = parity_status_from_row(row);
        if resp.status == "idle" || resp.status == "failed" {
            resp.status = "running".to_string();
        }
        return Ok((StatusCode::ACCEPTED, Json(resp)));
    }

    {
        let db = db_arc.lock().await;
        let _ = db.parity_result_set_running(&rule.name, now);
    }

    info!("Replication verify (background) started: rule='{}'", name);
    crate::audit::audit_log(
        "replication_verify",
        "admin",
        &name,
        &HeaderMap::new(),
        &rule.source.bucket,
        &rule.source.prefix,
    );

    // Detach the audit. It persists its own result + releases the lease.
    let engine = state.s3_state.engine.load().clone();
    let rule_clone = rule.clone();
    let db_for_task = db_arc.clone();
    tokio::spawn(async move {
        // Catch a panic in the audit so the lease + 'running' status are ALWAYS
        // settled — otherwise a panicked task would leave the lease stuck for the
        // full TTL and the UI polling a never-ending 'running' forever.
        let audit = std::panic::AssertUnwindSafe(replication::parity_audit(
            &engine,
            &rule_clone,
            replication::parity::MAX_PARITY_OBJECTS,
            Some(&db_for_task),
            Some(replication::parity::ParityProgress {
                db: &db_for_task,
                rule: &rule_clone.name,
            }),
        ));
        let audit = futures::FutureExt::catch_unwind(audit);
        // Heartbeat: renew the lease every TTL/3 so a scan that runs longer than
        // the TTL doesn't let a concurrent POST acquire + double-scan. The ticker
        // is cancelled (dropped) the moment the audit completes via select!.
        let heartbeat = async {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(
                (PARITY_LEASE_TTL_SECS / 3).max(1) as u64,
            ));
            tick.tick().await; // immediate first tick — skip
            loop {
                tick.tick().await;
                let now = crate::replication::current_unix_seconds();
                let db = db_for_task.lock().await;
                if !db
                    .parity_renew_lease(&rule_clone.name, &owner, now, PARITY_LEASE_TTL_SECS)
                    .unwrap_or(false)
                {
                    break; // lost the lease — stop renewing
                }
            }
        };
        let result = tokio::select! {
            r = audit => match r {
                Ok(r) => r,
                Err(_) => Err("parity audit panicked".to_string()),
            },
            _ = heartbeat => Err("parity audit lease lost".to_string()),
        };
        let now = crate::replication::current_unix_seconds();
        let db = db_for_task.lock().await;
        // Honour a cancel that landed in the final window (after the audit's
        // last in-scan check but before this terminal write): if the row is
        // 'cancelling', settle it 'cancelled' rather than overwriting with
        // 'done'. Read under the held lock so it's atomic with the write.
        let cancel_pending = matches!(
            db.parity_status(&rule_clone.name).ok().flatten(),
            Some(s) if s == "cancelling"
        );
        match result {
            Ok(_) if cancel_pending => {
                let _ = db.parity_result_cancelled(&rule_clone.name, now);
            }
            // Persist 'failed' (not a hollow 'done') if the outcome won't
            // serialize — a 'done' with empty outcome_json reads as no result.
            Ok(outcome) => match serde_json::to_string(&outcome) {
                Ok(json) => {
                    let _ = db.parity_result_done(&rule_clone.name, outcome.in_sync, &json, now);
                }
                Err(e) => {
                    let _ = db.parity_result_failed(
                        &rule_clone.name,
                        &format!("could not serialize parity result: {e}"),
                        now,
                    );
                }
            },
            Err(e) if e == replication::parity::CANCELLED => {
                let _ = db.parity_result_cancelled(&rule_clone.name, now);
            }
            Err(e) => {
                let _ = db.parity_result_failed(&rule_clone.name, &e, now);
            }
        }
        let _ = db.parity_release_lease(&rule_clone.name, &owner);
    });

    // Return the (now 'running') status immediately.
    let row = {
        let db = db_arc.lock().await;
        db.parity_result_load(&rule.name).ok().flatten()
    };
    Ok((StatusCode::ACCEPTED, Json(parity_status_from_row(row))))
}

/// GET: poll the current parity audit status / last result (server-side, so it
/// survives navigation + restart). No scan is started here.
pub async fn verify_status(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<ParityStatusResponse>, (StatusCode, String)> {
    let _ = snapshot_and_find_rule(&state, &name).await?;
    let row = match &state.config_db {
        Some(db_arc) => {
            let db = db_arc.lock().await;
            db.parity_result_load(&name).ok().flatten()
        }
        None => None,
    };
    Ok(Json(parity_status_from_row(row)))
}

/// POST: request cancellation of a running parity audit. Flips the row to
/// 'cancelling'; the background scan polls this between LIST pages and bails,
/// settling the row to 'cancelled'. Returns the current status either way
/// (idempotent — cancelling an idle/done audit is a no-op).
pub async fn verify_cancel(
    Path(name): Path<String>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<ParityStatusResponse>, (StatusCode, String)> {
    let _ = snapshot_and_find_rule(&state, &name).await?;
    let row = match &state.config_db {
        Some(db_arc) => {
            let db = db_arc.lock().await;
            let now = crate::replication::current_unix_seconds();
            let _ = db.parity_request_cancel(&name, now);
            db.parity_result_load(&name).ok().flatten()
        }
        None => None,
    };
    Ok(Json(parity_status_from_row(row)))
}

/// Background-job lease TTL for a parity audit. Long enough to cover a large
/// scan; a crash clears it on the next boot reconcile.
const PARITY_LEASE_TTL_SECS: i64 = 1800;

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
