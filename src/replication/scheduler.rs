//! Periodic replication scheduler.
//!
//! The admin API owns explicit "run now" execution. This module owns the
//! background loop that wakes up, discovers due rules from the hot-reloaded
//! config + config DB state, and runs them through the same worker path.

use crate::api::handlers::AppState;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::ReplicationConfig;
use crate::replication::{current_unix_seconds, run_rule, RunLease};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_TICK: Duration = Duration::from_secs(30);
const MIN_TICK: Duration = Duration::from_secs(5);
const DEFAULT_LEASE_TTL_SECS: i64 = 60;
const DEFAULT_HEARTBEAT_SECS: i64 = 20;

pub fn spawn_scheduler(
    config: SharedConfig,
    db: Arc<Mutex<ConfigDb>>,
    state: Arc<AppState>,
) -> tokio::task::JoinHandle<()> {
    let instance_id = format!("scheduler:{}", uuid::Uuid::new_v4());
    tokio::spawn(async move {
        info!("Replication scheduler started: instance_id={}", instance_id);
        loop {
            let tick = {
                let cfg = config.read().await;
                scheduler_tick(&cfg.replication)
            };
            tokio::time::sleep(tick).await;

            let replication = { config.read().await.replication.clone() };
            if replication.enabled {
                run_due_rules(&replication, &db, &state, &instance_id).await;
            } else {
                debug!("Replication scheduler skipped: global replication disabled");
            }
        }
    })
}

async fn run_due_rules(
    replication: &ReplicationConfig,
    db: &Arc<Mutex<ConfigDb>>,
    state: &Arc<AppState>,
    instance_id: &str,
) {
    for rule in replication.rules.iter().filter(|rule| rule.enabled) {
        let now = current_unix_seconds();
        let should_run = {
            let db = db.lock().await;
            if let Err(err) = db.replication_ensure_state(&rule.name, now) {
                warn!(
                    "Replication scheduler could not initialise state for rule '{}': {}",
                    rule.name, err
                );
                false
            } else {
                match db.replication_load_state(&rule.name) {
                    Ok(Some(st)) if st.paused => {
                        debug!("Replication scheduler skipped paused rule '{}'", rule.name);
                        false
                    }
                    Ok(Some(st)) if st.next_due_at > now => false,
                    Ok(Some(_)) | Ok(None) => match db.replication_try_acquire_lease(
                        &rule.name,
                        instance_id,
                        now,
                        lease_ttl_secs(replication),
                    ) {
                        Ok(true) => true,
                        Ok(false) => {
                            debug!("Replication scheduler skipped busy rule '{}'", rule.name);
                            false
                        }
                        Err(err) => {
                            warn!(
                                "Replication scheduler could not acquire lease for rule '{}': {}",
                                rule.name, err
                            );
                            false
                        }
                    },
                    Err(err) => {
                        warn!(
                            "Replication scheduler could not load state for rule '{}': {}",
                            rule.name, err
                        );
                        false
                    }
                }
            }
        };

        if !should_run {
            continue;
        }

        info!("Replication scheduler running due rule '{}'", rule.name);
        let engine = state.engine.load().clone();
        if let Err(err) = run_rule(
            db.clone(),
            &engine,
            rule,
            replication.max_failures_retained,
            "scheduler",
            Some(RunLease {
                owner: instance_id.to_string(),
                ttl_secs: lease_ttl_secs(replication),
                heartbeat_secs: heartbeat_secs(replication),
            }),
        )
        .await
        {
            warn!(
                "Replication scheduler failed to run rule '{}': {}",
                rule.name, err
            );
        }
        {
            let db = db.lock().await;
            let _ = db.replication_release_lease(&rule.name, instance_id);
        }
    }
}

pub(crate) fn scheduler_tick(replication: &ReplicationConfig) -> Duration {
    parse_duration_or(
        &replication.tick_interval,
        DEFAULT_TICK,
        MIN_TICK,
        "replication.tick_interval",
    )
}

pub(crate) fn lease_ttl_secs(replication: &ReplicationConfig) -> i64 {
    parse_duration_or(
        &replication.lease_ttl,
        Duration::from_secs(DEFAULT_LEASE_TTL_SECS as u64),
        Duration::from_secs(1),
        "replication.lease_ttl",
    )
    .as_secs() as i64
}

pub(crate) fn heartbeat_secs(replication: &ReplicationConfig) -> i64 {
    let heartbeat = parse_duration_or(
        &replication.heartbeat_interval,
        Duration::from_secs(DEFAULT_HEARTBEAT_SECS as u64),
        Duration::from_secs(1),
        "replication.heartbeat_interval",
    )
    .as_secs() as i64;
    let ttl = lease_ttl_secs(replication);
    if ttl > 1 && heartbeat >= ttl {
        let clamped = (ttl / 2).max(1);
        warn!(
            "replication.heartbeat_interval={} is not below lease_ttl {}; using {}s",
            replication.heartbeat_interval, ttl, clamped
        );
        clamped
    } else {
        heartbeat
    }
}

fn parse_duration_or(value: &str, default: Duration, minimum: Duration, label: &str) -> Duration {
    match humantime::parse_duration(value) {
        Ok(duration) if duration >= minimum => duration,
        Ok(duration) => {
            warn!(
                "{}={} below minimum {}; using {}",
                label,
                humantime::format_duration(duration),
                humantime::format_duration(minimum),
                humantime::format_duration(minimum),
            );
            minimum
        }
        Err(err) => {
            warn!(
                "{}={} invalid: {}; using {}",
                label,
                value,
                err,
                humantime::format_duration(default),
            );
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::ReplicationConfig;

    #[test]
    fn scheduler_tick_uses_configured_duration() {
        let cfg = ReplicationConfig {
            tick_interval: "45s".to_string(),
            ..ReplicationConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), Duration::from_secs(45));
    }

    #[test]
    fn scheduler_tick_clamps_too_small_duration() {
        let cfg = ReplicationConfig {
            tick_interval: "1s".to_string(),
            ..ReplicationConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), MIN_TICK);
    }

    #[test]
    fn scheduler_tick_falls_back_on_invalid_duration() {
        let cfg = ReplicationConfig {
            tick_interval: "wat".to_string(),
            ..ReplicationConfig::default()
        };
        assert_eq!(scheduler_tick(&cfg), DEFAULT_TICK);
    }

    #[test]
    fn lease_timing_uses_configured_durations() {
        let cfg = ReplicationConfig {
            lease_ttl: "75s".to_string(),
            heartbeat_interval: "25s".to_string(),
            ..ReplicationConfig::default()
        };
        assert_eq!(lease_ttl_secs(&cfg), 75);
        assert_eq!(heartbeat_secs(&cfg), 25);
    }

    #[test]
    fn heartbeat_is_clamped_below_lease_ttl() {
        let cfg = ReplicationConfig {
            lease_ttl: "10s".to_string(),
            heartbeat_interval: "10s".to_string(),
            ..ReplicationConfig::default()
        };
        assert_eq!(heartbeat_secs(&cfg), 5);
    }
}
