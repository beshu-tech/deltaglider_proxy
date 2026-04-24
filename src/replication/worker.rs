//! Replication worker: executes one run of a single rule against a
//! live engine + config DB. The scheduler (future) calls `run_rule`
//! in due order; the admin `run-now` endpoint also calls it directly.
//!
//! Today this is a one-shot `run_rule` that:
//! 1. Lists source objects (single bounded page for the v1 shim).
//! 2. Per-object: HEAD the destination, consult the planner, copy via
//!    the engine.
//! 3. Records per-object failures into the failure ring.
//! 4. Writes totals + status back via `replication_finish_run`.
//!
//! Resumable pagination (continuation_token) and delete replication
//! come in a follow-up commit — the worker is structured so they drop
//! in as additional paths through `run_rule`.

use super::planner::plan_batch;
use super::state_store::{current_unix_seconds, RunTotals};
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationRule;
use crate::deltaglider::DynEngine;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Outcome of a single run (excluding the ones marked failed/cancelled
/// externally — callers translate this into a `finish_run` call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// Terminal status string (goes into `replication_run_history.status`).
    pub status: String,
    pub totals: RunTotals,
}

/// Execute a single run of a rule. Inserts a begin_run row, performs
/// the copy pass, records per-object failures, and calls finish_run.
///
/// Takes `Arc<Mutex<ConfigDb>>` rather than a `&ConfigDb` reference
/// because `rusqlite::Connection` is `!Sync`: holding a
/// `MutexGuard<ConfigDb>` across `.await` yields a `!Send` future,
/// which axum handlers cannot accept. By reacquiring the lock for
/// each sync boundary the run future stays `Send`. Engine awaits
/// happen between lock-drops.
///
/// `max_failures_retained` is the ring-cap — from
/// ReplicationConfig.max_failures_retained.
///
/// Returns the run id + terminal outcome so callers (admin API) can
/// surface it in the response.
pub async fn run_rule(
    db: Arc<Mutex<ConfigDb>>,
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    max_failures_retained: u32,
) -> Result<(i64, RunOutcome), crate::config_db::ConfigDbError> {
    let started_at = current_unix_seconds();
    let run_id = {
        let db = db.lock().await;
        db.replication_ensure_state(&rule.name, started_at)?;
        db.replication_begin_run(&rule.name, started_at)?
    };

    info!(
        "Replication run starting: rule='{}' src={}/{} dst={}/{}",
        rule.name,
        rule.source.bucket,
        rule.source.prefix,
        rule.destination.bucket,
        rule.destination.prefix
    );

    let mut totals = RunTotals::default();
    let mut status = "succeeded".to_string();

    // Single-page copy pass. We take whatever fits in `batch_size`,
    // attempt each, then finish. Multi-page + continuation token come
    // later.
    let cap = rule.batch_size.clamp(1, 10_000);
    let page = match engine
        .list_objects(&rule.source.bucket, &rule.source.prefix, None, cap, None, true)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "replication rule '{}' list failed: {}",
                rule.name, e
            );
            {
                let db = db.lock().await;
                db.replication_record_failure(
                    &rule.name,
                    current_unix_seconds(),
                    "",
                    "",
                    &format!("list source failed: {}", e),
                    max_failures_retained,
                )?;
            }
            totals.errors = 1;
            status = "failed".to_string();
            let finished = current_unix_seconds();
            let next_due = finished + 60;
            {
                let db = db.lock().await;
                db.replication_finish_run(run_id, &rule.name, &status, finished, totals, next_due)?;
            }
            return Ok((run_id, RunOutcome { status, totals }));
        }
    };

    totals.objects_scanned = page.objects.len() as i64;

    // Decide per-object via the planner. head_dest is an async closure
    // that queries engine.head on the destination bucket.
    let plan = {
        let head_engine = engine.clone();
        let dest_bucket = rule.destination.bucket.clone();
        plan_batch(&page.objects, rule, move |dest_key| {
            let engine = head_engine.clone();
            let dest_bucket = dest_bucket.clone();
            let dk = dest_key.to_string();
            async move { engine.head(&dest_bucket, &dk).await.ok() }
        })
        .await
    };

    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "replication rule '{}' planner error: {}",
                rule.name, e
            );
            {
                let db = db.lock().await;
                db.replication_record_failure(
                    &rule.name,
                    current_unix_seconds(),
                    "",
                    "",
                    &format!("planner error: {}", e),
                    max_failures_retained,
                )?;
            }
            totals.errors += 1;
            status = "failed".to_string();
            let finished = current_unix_seconds();
            let next_due = finished + 60;
            {
                let db = db.lock().await;
                db.replication_finish_run(run_id, &rule.name, &status, finished, totals, next_due)?;
            }
            return Ok((run_id, RunOutcome { status, totals }));
        }
    };

    totals.objects_skipped = plan.skipped.len() as i64;

    // Execute the copies.
    for (src_key, dest_key) in &plan.to_copy {
        match copy_one(engine, rule, src_key, dest_key).await {
            Ok(bytes_copied) => {
                totals.objects_copied += 1;
                totals.bytes_copied += bytes_copied as i64;
            }
            Err(e) => {
                totals.errors += 1;
                {
                    let db = db.lock().await;
                    db.replication_record_failure(
                        &rule.name,
                        current_unix_seconds(),
                        src_key,
                        dest_key,
                        &format!("{}", e),
                        max_failures_retained,
                    )?;
                }
                debug!(
                    "replication rule '{}' object failure src={:?} dst={:?}: {}",
                    rule.name, src_key, dest_key, e
                );
            }
        }
    }

    // If every copy errored out, mark the run failed.
    if !plan.to_copy.is_empty() && totals.objects_copied == 0 && totals.errors > 0 {
        status = "failed".to_string();
    }

    let finished_at = current_unix_seconds();
    let next_due = compute_next_due(rule, finished_at);
    {
        let db = db.lock().await;
        db.replication_finish_run(run_id, &rule.name, &status, finished_at, totals, next_due)?;
    }

    info!(
        "Replication run finished: rule='{}' status={} scanned={} copied={} skipped={} errors={} bytes={}",
        rule.name,
        status,
        totals.objects_scanned,
        totals.objects_copied,
        totals.objects_skipped,
        totals.errors,
        totals.bytes_copied,
    );
    Ok((run_id, RunOutcome { status, totals }))
}

/// Copy a single object: engine.retrieve from source, engine.store on
/// destination. Returns bytes copied.
async fn copy_one(
    engine: &Arc<DynEngine>,
    rule: &ReplicationRule,
    src_key: &str,
    dest_key: &str,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    // HEAD first so we can error cleanly if the source disappeared
    // mid-run (retrieve would also error, but this gives a crisper
    // cause on the failure row).
    let _src_meta = engine
        .head(&rule.source.bucket, src_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source head failed: {}", e).into()
        })?;

    let (data, meta) = engine
        .retrieve(&rule.source.bucket, src_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source retrieve failed: {}", e).into()
        })?;

    let content_type = meta.content_type.clone();
    let user_metadata = meta.user_metadata.clone();
    let bytes = data.len();

    engine
        .store(
            &rule.destination.bucket,
            dest_key,
            &data,
            content_type,
            user_metadata,
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("destination store failed: {}", e).into()
        })?;

    Ok(bytes)
}

/// Compute when this rule should next be due. Falls back to a 1-hour
/// recovery window if the rule's `interval` is unparseable (should
/// never happen in practice — validated at Config::check time).
fn compute_next_due(rule: &ReplicationRule, finished_at: i64) -> i64 {
    match humantime::parse_duration(&rule.interval) {
        Ok(d) => finished_at + d.as_secs() as i64,
        Err(_) => finished_at + 3600,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::{ConflictPolicy, ReplicationEndpoint, ReplicationRule};

    fn mk_rule() -> ReplicationRule {
        ReplicationRule {
            name: "r".to_string(),
            enabled: true,
            source: ReplicationEndpoint {
                bucket: "a".into(),
                prefix: String::new(),
            },
            destination: ReplicationEndpoint {
                bucket: "b".into(),
                prefix: String::new(),
            },
            interval: "1h".into(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::NewerWins,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".into()],
        }
    }

    #[test]
    fn compute_next_due_honours_interval() {
        let rule = mk_rule();
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }

    #[test]
    fn compute_next_due_falls_back_on_invalid() {
        let mut rule = mk_rule();
        rule.interval = "garbage".into();
        // 1h fallback.
        assert_eq!(compute_next_due(&rule, 1000), 1000 + 3600);
    }
}
