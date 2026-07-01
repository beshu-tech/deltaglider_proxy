// SPDX-License-Identifier: GPL-3.0-only

//! Conservative lifecycle scheduler.

use crate::api::handlers::AppState;
use crate::background::parse_duration_or;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::LifecycleConfig;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_TICK: Duration = Duration::from_secs(3600);
const MIN_TICK: Duration = Duration::from_secs(60);
// Lifecycle leases are deliberately 5x longer than replication's
// (TTL 300s/heartbeat 60s here vs 60s/20s in `replication::scheduler`).
// The tick cadence differs by the same order of magnitude: lifecycle wakes
// at most once a minute (MIN_TICK) and typically hourly (DEFAULT_TICK),
// whereas replication wakes every few seconds. A single lifecycle run also
// does heavier, slower work (full prefix scans + deletes through the engine),
// so a longer TTL keeps the lease alive across a slow run without a peer
// stealing it mid-flight. The tradeoff: if the lease holder crashes, another
// instance waits up to TTL seconds before taking over — acceptable given how
// rarely lifecycle runs. Heartbeat stays well under TTL so a live-but-slow run
// keeps refreshing the lease.
const DEFAULT_LEASE_TTL_SECS: i64 = 300;
const DEFAULT_HEARTBEAT_SECS: i64 = 60;

pub fn spawn_scheduler(
    config: SharedConfig,
    db: Option<Arc<Mutex<ConfigDb>>>,
    state: Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    let instance_id = format!("lifecycle-scheduler:{}", uuid::Uuid::new_v4());
    tokio::spawn(async move {
        info!("Lifecycle scheduler started: instance_id={}", instance_id);
        loop {
            let tick = {
                let cfg = config.read().await;
                scheduler_tick(&cfg.lifecycle)
            };
            tokio::time::sleep(tick).await;

            let lifecycle = { config.read().await.lifecycle.clone() };
            if lifecycle.enabled {
                run_due_rules(&lifecycle, db.clone(), &state, &instance_id).await;
            } else {
                debug!("Lifecycle scheduler skipped: global lifecycle disabled");
            }
        }
    })
}

async fn run_due_rules(
    lifecycle: &LifecycleConfig,
    db: Option<Arc<Mutex<ConfigDb>>>,
    state: &Arc<AppState>,
    instance_id: &str,
) {
    // Duplicate names share one lifecycle_state row (keyed by name): the second
    // rule is silently starved and both corrupt the cursor — skip the WHOLE set.
    let dup_names =
        super::planner::duplicate_rule_names(lifecycle.rules.iter().filter(|r| r.enabled));
    for rule in lifecycle.rules.iter().filter(|rule| rule.enabled) {
        if dup_names.contains(&rule.name) {
            warn!(
                "Lifecycle scheduler skipping rule '{}': name is duplicated across enabled rules \
                 (state/cursor/lease are keyed by name)",
                rule.name
            );
            continue;
        }
        // Config-time fatal gate: a rule that can NEVER run (delete/transition
        // missing·invalid·out-of-range expire_after, retain-newest count=0,
        // bad globs/durations) is skipped here — not run-and-failed every
        // tick (the origin of the recurring Jobs-FAILED row). The same fn is
        // enforced at /config/apply + `config lint`, so a new bad config is
        // rejected before it reaches the scheduler; this guard is the
        // defence for a rule already present in a YAML file at startup.
        let fatal = super::planner::lifecycle_rule_errors(rule);
        if !fatal.is_empty() {
            warn!(
                "Lifecycle scheduler skipping invalid rule '{}': {}",
                rule.name,
                fatal.join("; ")
            );
            continue;
        }
        let now = super::current_unix_seconds();
        let mut process_guard = None;
        let should_run = if let Some(db) = db.as_ref() {
            let db_guard = db.lock().await;
            if let Err(err) = db_guard.lifecycle_ensure_state(&rule.name, now) {
                warn!(
                    "Lifecycle scheduler could not initialise state for rule '{}': {}",
                    rule.name, err
                );
                false
            } else {
                match db_guard.lifecycle_load_state(&rule.name) {
                    Ok(Some(st)) if st.paused => {
                        debug!("Lifecycle scheduler skipped paused rule '{}'", rule.name);
                        false
                    }
                    Ok(Some(st)) if st.next_due_at > now => false,
                    Ok(Some(_)) | Ok(None) => match db_guard.lifecycle_try_acquire_lease(
                        &rule.name,
                        instance_id,
                        now,
                        lease_ttl_secs(),
                    ) {
                        Ok(true) => true,
                        Ok(false) => {
                            debug!("Lifecycle scheduler skipped busy rule '{}'", rule.name);
                            false
                        }
                        Err(err) => {
                            warn!(
                                "Lifecycle scheduler could not acquire lease for rule '{}': {}",
                                rule.name, err
                            );
                            false
                        }
                    },
                    Err(err) => {
                        warn!(
                            "Lifecycle scheduler could not load state for rule '{}': {}",
                            rule.name, err
                        );
                        false
                    }
                }
            }
        } else {
            match super::try_acquire_rule(&rule.name) {
                Some(guard) => {
                    process_guard = Some(guard);
                    true
                }
                None => {
                    debug!("Lifecycle scheduler skipped busy rule '{}'", rule.name);
                    false
                }
            }
        };
        if !should_run {
            continue;
        }

        // Source AND transition destination: a transition PUT landing on
        // a gated bucket is the racing write the gate exists to stop.
        if let Some(busy) = super::planner::rule_write_buckets(rule)
            .into_iter()
            .find(|b| state.maintenance_gate.is_busy(b))
        {
            info!(
                "Lifecycle scheduler deferring rule '{}': bucket '{}' is under maintenance",
                rule.name, busy
            );
            continue;
        }

        info!("Lifecycle scheduler running rule '{}'", rule.name);
        let engine = state.engine.load().clone();
        match super::run_rule(
            db.clone(),
            &engine,
            rule,
            lifecycle.max_failures_retained,
            "scheduler",
            scheduler_tick(lifecycle).as_secs() as i64,
            Some(super::RunLease {
                owner: instance_id.to_string(),
                ttl_secs: lease_ttl_secs(),
                heartbeat_secs: heartbeat_secs(),
            }),
        )
        .await
        {
            Ok(outcome) if outcome.errors == 0 => {
                info!(
                    "Lifecycle rule '{}' completed: affected={} scanned={}",
                    rule.name, outcome.objects_affected, outcome.objects_scanned
                );
            }
            Ok(outcome) => {
                warn!(
                    "Lifecycle rule '{}' completed with {} errors (affected={}, scanned={})",
                    rule.name, outcome.errors, outcome.objects_affected, outcome.objects_scanned
                );
            }
            Err(err) => warn!("Lifecycle rule '{}' failed: {}", rule.name, err),
        }
        if let Some(db) = db.as_ref() {
            let db = db.lock().await;
            let _ = db.lifecycle_release_lease(&rule.name, instance_id);
        }
        drop(process_guard);
    }
}

pub(crate) fn scheduler_tick(lifecycle: &LifecycleConfig) -> Duration {
    parse_duration_or(
        &lifecycle.tick_interval,
        DEFAULT_TICK,
        MIN_TICK,
        "lifecycle.tick_interval",
    )
}

pub(crate) fn lease_ttl_secs() -> i64 {
    DEFAULT_LEASE_TTL_SECS
}

pub(crate) fn heartbeat_secs() -> i64 {
    DEFAULT_HEARTBEAT_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_tick_uses_configured_duration() {
        let cfg = LifecycleConfig {
            tick_interval: "2h".to_string(),
            ..LifecycleConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), Duration::from_secs(7200));
    }

    #[test]
    fn scheduler_tick_clamps_too_small_duration() {
        let cfg = LifecycleConfig {
            tick_interval: "1s".to_string(),
            ..LifecycleConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), MIN_TICK);
    }

    #[test]
    fn scheduler_tick_falls_back_on_invalid_duration() {
        let cfg = LifecycleConfig {
            tick_interval: "wat".to_string(),
            ..LifecycleConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), DEFAULT_TICK);
    }
}
