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
use tracing::{info, warn};

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
) -> Result<(StatusCode, Json<RunNowResponse>), (StatusCode, String)> {
    let (repl, rule) = snapshot_and_find_rule(&state, &name).await?;

    // The GLOBAL kill-switch (`replication.enabled`) stays a hard block — it's
    // the master off-switch (e.g. frozen during a migration).
    if !repl.enabled {
        return Err((
            StatusCode::CONFLICT,
            "replication is globally disabled (storage.replication.enabled = false)".to_string(),
        ));
    }
    // A manual run-now is a deliberate ONE-OFF: it runs the rule once even when
    // the rule is `enabled: false` or paused. It does NOT flip either flag — the
    // scheduler stays off, so this is a single operator-triggered sync, not a
    // silent re-enable. (The per-rule disabled/paused 409s are intentionally
    // gone; the operator asked for exactly this affordance.)

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

    // Cross-backend liveness gate (H14): when a coordination (S3) lease is
    // active, a scheduled run holds ONLY that lease — the node-local SQLite
    // acquire below would succeed (SQLite lease free) and DOUBLE-RUN. Check the
    // active lease first; a held lease → 409. (No-op when only LocalLease is
    // wired, which the SQLite acquire already covers.)
    if let Some(lease) = state.coordination_lease.as_ref() {
        let now = replication::current_unix_seconds();
        if lease
            .is_held(
                crate::coordination::LeaseSubsystem::Replication,
                &rule.name,
                now,
            )
            .await
            .unwrap_or(false)
        {
            return Err((
                StatusCode::CONFLICT,
                "rule is already running; wait for the current run to finish".to_string(),
            ));
        }
    }

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
        // NOTE: no paused check — a manual run-now is a deliberate one-off that
        // runs even a paused rule once (see the header comment). The lease we
        // just took serializes it against a concurrent run; the run settles and
        // releases the lease normally, leaving `paused` untouched.
    }

    // Post-acquire re-check: delete_rule holds the config lock while it checks
    // our lease, so if the rule vanished here we lost that race — back out.
    if !state
        .config
        .read()
        .await
        .replication
        .rules
        .iter()
        .any(|r| r.name == rule.name)
    {
        let db = db_arc.lock().await;
        let _ = db.replication_release_lease(&rule.name, &lease_owner);
        return Err((StatusCode::NOT_FOUND, "rule was deleted".to_string()));
    }

    info!("Replication run-now via admin API: rule='{}'", name);

    crate::audit::audit_log(
        "replication_run_now",
        "admin",
        &name,
        &HeaderMap::new(),
        &rule.source.bucket,
        &rule.source.prefix,
    );

    // Run in the BACKGROUND and return immediately. A replication run can copy
    // many GB (streaming multipart) and take minutes — blocking the HTTP request
    // until it finishes hangs the admin AJAX for the whole sync. We already hold
    // the lease (acquired above under the DB lock, so a double run-now still
    // 409s); the spawned task owns the run and releases the lease when done. The
    // run appears in the jobs list as `running` with live progress via the
    // existing run-history polling — the client polls, it does not wait here.
    let engine = state.s3_state.engine.load().clone();
    let maintenance_gate = state.s3_state.maintenance_gate.clone();
    let max_failures_retained = repl.max_failures_retained;
    let object_timeout = replication::scheduler::object_timeout(&repl);
    let object_skip_after_failures = repl.object_skip_after_failures;
    let lease_ttl_secs = replication::scheduler::lease_ttl_secs(&repl);
    let heartbeat_secs = replication::scheduler::heartbeat_secs(&repl);
    let concurrency = replication::RunConcurrency {
        transfers: repl.transfers,
        upload_concurrency: repl.upload_concurrency,
        dir_concurrency: repl.dir_concurrency,
    };
    let rule_owned = rule.clone();
    tokio::spawn(async move {
        let result = replication::run_rule(
            db_arc.clone(),
            &engine,
            &rule_owned,
            max_failures_retained,
            object_timeout,
            object_skip_after_failures,
            "run-now",
            Some(replication::RunLease {
                owner: lease_owner.clone(),
                ttl_secs: lease_ttl_secs,
                heartbeat_secs,
            }),
            concurrency,
            Some(maintenance_gate),
            // Admin run-now uses the node-local SQLite lease (it is an explicit,
            // sticky-routed operator action) — not the cross-instance trait lease.
            None,
        )
        .await;
        {
            let db = db_arc.lock().await;
            let _ = db.replication_release_lease(&rule_owned.name, &lease_owner);
        }
        if let Err(e) = result {
            warn!(
                "Replication run-now background task failed: rule='{}': {}",
                rule_owned.name, e
            );
        }
    });

    // 202 Accepted — the run started; poll the jobs list / run history for
    // progress and the final outcome. Totals are 0 here (not yet known).
    Ok((
        StatusCode::ACCEPTED,
        Json(RunNowResponse {
            run_id: 0,
            status: "running".to_string(),
            objects_scanned: 0,
            objects_copied: 0,
            objects_skipped: 0,
            bytes_copied: 0,
            errors: 0,
        }),
    ))
}

/// The poll envelope for the parity audit (a background job). `status` is
/// idle | running | done | failed. `outcome` is the last completed verdict
/// (kept while a new scan runs). The frontend polls `GET verify`.
#[derive(serde::Serialize)]
pub struct ParityStatusResponse {
    pub status: String,
    pub progress_scanned: i64,
    /// Compare-phase denominator (0 = unknown → indeterminate bar).
    pub progress_total: i64,
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
            progress_total: 0,
            scanned_at: None,
            outcome: None,
            error: None,
        };
    };
    // While a scan is IN FLIGHT, the persisted `outcome_json` (and its
    // scanned_at) belong to the PREVIOUS completed audit. Surfacing it makes a
    // running/cancelling verify look like it already found those (stale)
    // differences — the exact confusion that got a slow run killed. Suppress the
    // stale outcome until the current scan settles; the UI then shows live
    // progress (its `running && !outcome` guard) instead of a phantom verdict.
    let in_flight = matches!(row.status.as_str(), "running" | "cancelling");
    let outcome = if in_flight {
        None
    } else {
        row.outcome_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
    };
    ParityStatusResponse {
        status: row.status,
        progress_scanned: row.progress_scanned,
        progress_total: row.progress_total,
        // scanned_at also describes the PREVIOUS result — hide it while in flight
        // so the UI never pairs a live scan with a stale timestamp.
        scanned_at: if in_flight { None } else { row.scanned_at },
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
            replication::parity::max_parity_objects(),
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
                progress_total: outcome.source_objects as i64,
                scanned_at: Some(outcome.scanned_at),
                outcome: Some(outcome),
                error: None,
            }),
        ));
    };

    let owner = format!("verify:{}", uuid::Uuid::new_v4());
    let now = crate::replication::current_unix_seconds();
    // A verify DURING a replication run of the same rule is meaningless — the
    // dest is expected to be mid-sync — and the UI's "fix then verify" flow
    // (run-now → verify) would otherwise race a false 'not in sync' verdict.
    // Refuse with 409 while the run lease is held (finding #17).
    {
        let db = db_arc.lock().await;
        if db
            .replication_lease_is_held(&rule.name, now)
            .unwrap_or(false)
        {
            return Err((
                StatusCode::CONFLICT,
                "a replication run is in progress for this rule — verify after it settles"
                    .to_string(),
            ));
        }
    }
    // Cross-backend liveness gate (H48): a scheduled run under a coordination
    // (S3) lease is invisible to the SQLite check above — consult the active
    // lease too so verify doesn't run a parity audit against a mid-sync dest.
    if let Some(lease) = state.coordination_lease.as_ref() {
        if lease
            .is_held(
                crate::coordination::LeaseSubsystem::Replication,
                &rule.name,
                now,
            )
            .await
            .unwrap_or(false)
        {
            return Err((
                StatusCode::CONFLICT,
                "a replication run is in progress for this rule — verify after it settles"
                    .to_string(),
            ));
        }
    }
    // Acquire the lease; if someone else holds it, just report current status.
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
        // Seed the progress bar with the PRIOR run's object count so it goes
        // determinate immediately (kills the "looked frozen" indeterminate span
        // during listing); the driver's real set_total corrects it. First-ever
        // run has no prior → 0 → indeterminate, as before.
        let est_total = db
            .parity_result_load(&rule.name)
            .ok()
            .flatten()
            .and_then(|r| r.outcome_json)
            .and_then(|j| serde_json::from_str::<replication::ParityOutcome>(&j).ok())
            .map(|o| (o.source_objects + o.dest_objects) as i64)
            .unwrap_or(0);
        let _ = db.parity_result_set_running(&rule.name, est_total, now);
    }
    // NOTE: no sync push — coordination tables (leases, parity, run history)
    // are NODE-LOCAL (dropped from the IAM sync set in B3), so a push cannot
    // make a peer see this lease. Two instances CAN double-scan; see CLAUDE.md.

    info!("Replication verify (background) started: rule='{}'", name);
    crate::audit::audit_log(
        "replication_verify",
        "admin",
        &name,
        &HeaderMap::new(),
        &rule.source.bucket,
        &rule.source.prefix,
    );

    // In-process cancel flag for a fast (lock-free) abort. Registered for the
    // duration of the scan; removed on settle. The durable 'cancelling' DB row
    // stays the cross-instance / post-restart signal.
    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut map = state.parity_cancels.lock().unwrap();
        map.insert(rule.name.clone(), cancel_flag.clone());
    }

    // Detach the audit. It persists its own result + releases the lease.
    // ponytail: this one-shot detached + select-heartbeat + catch_unwind + settle
    // orchestration is the ONLY instance of its shape (lease primitives are already
    // shared via config_db::job_store; maintenance is a different poll-loop shape).
    // Don't extract a BackgroundJob driver until a genuine 2nd one-shot task lands.
    let engine = state.s3_state.engine.load().clone();
    let rule_clone = rule.clone();
    let db_for_task = db_arc.clone();
    let cancels = state.parity_cancels.clone();
    tokio::spawn(async move {
        // Catch a panic in the audit so the lease + 'running' status are ALWAYS
        // settled — otherwise a panicked task would leave the lease stuck for the
        // full TTL and the UI polling a never-ending 'running' forever.
        let audit = std::panic::AssertUnwindSafe(replication::parity_audit(
            &engine,
            &rule_clone,
            replication::parity::max_parity_objects(),
            Some(&db_for_task),
            Some(replication::parity::ParityProgress {
                db: &db_for_task,
                rule: &rule_clone.name,
                cancel: &cancel_flag,
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
        drop(db);
        // Bump AFTER the terminal row is written, so a test polling parity-version
        // that sees the new count also sees the settled row.
        replication::parity::bump_parity_version();
        // Deregister this run's cancel flag (only if it's still ours — a new run
        // may have replaced it). Removing a flag the registry no longer points to
        // is harmless; guard on identity to avoid evicting a successor's flag.
        let mut map = cancels.lock().unwrap();
        if map
            .get(&rule_clone.name)
            .is_some_and(|f| Arc::ptr_eq(f, &cancel_flag))
        {
            map.remove(&rule_clone.name);
        }
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
            // Self-heal a zombie 'running' row (dead task, no boot since) before
            // the read, so the UI doesn't poll a stuck spinner forever.
            let now = crate::replication::current_unix_seconds();
            let _ = db.parity_reap_if_dead(&name, now);
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
    // Fast in-process signal: a local scan checks this every page without a lock.
    if let Some(flag) = state.parity_cancels.lock().unwrap().get(&name) {
        flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let row = match &state.config_db {
        Some(db_arc) => {
            let db = db_arc.lock().await;
            let now = crate::replication::current_unix_seconds();
            // Durable signal — survives restart + reaches another instance.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replication::parity::{ParityOutcome, Regime};
    use crate::replication::ParityResultRow;

    /// Any valid serialized outcome — the suppression test only cares that a
    /// non-None outcome exists, not its contents. Built from the typed struct so
    /// a field change breaks at compile time, not via a hand-edited JSON literal.
    fn some_outcome_json() -> String {
        let o = ParityOutcome {
            rule_name: "r".into(),
            source_bucket: "s".into(),
            dest_bucket: "d".into(),
            source_objects: 0,
            dest_objects: 0,
            matched: 1,
            missing_on_dest: 0,
            orphan_on_dest: 0,
            checksum_mismatch: 0,
            unverifiable: 0,
            truncated: false,
            cap_hit: false,
            unresolved: 0,
            in_sync: true,
            scanned_at: 1000,
            regime: Regime::Transforming,
            conflict_policy: crate::config_sections::ConflictPolicy::ContentDiff,
            replicate_deletes: false,
            actionable: Default::default(),
            missing_samples: vec![],
            orphan_samples: vec![],
            mismatch_samples: vec![],
            verdict: crate::replication::parity::Verdict::Safe,
            verdict_summary: "All 1 objects are present and verified identical.".into(),
        };
        serde_json::to_string(&o).expect("serialize test outcome")
    }

    fn row(status: &str, has_outcome: bool) -> ParityResultRow {
        ParityResultRow {
            status: status.to_string(),
            scanned_at: Some(1000),
            progress_scanned: 5,
            progress_total: 10,
            outcome_json: has_outcome.then(some_outcome_json),
            last_error: None,
        }
    }

    #[test]
    fn stale_outcome_is_suppressed_while_scan_in_flight() {
        // A settled scan surfaces its outcome + timestamp.
        let done = parity_status_from_row(Some(row("done", true)));
        assert!(done.outcome.is_some(), "done → outcome shown");
        assert_eq!(done.scanned_at, Some(1000));

        // A running/cancelling scan must NOT surface the PREVIOUS outcome or its
        // timestamp — else the UI renders a phantom (stale) verdict mid-scan.
        for st in ["running", "cancelling"] {
            let r = parity_status_from_row(Some(row(st, true)));
            assert!(r.outcome.is_none(), "{st}: stale outcome must be hidden");
            assert_eq!(r.scanned_at, None, "{st}: stale timestamp must be hidden");
            // Progress still flows so the UI shows a live bar.
            assert_eq!(r.progress_scanned, 5);
            assert_eq!(r.progress_total, 10);
        }
    }
}
