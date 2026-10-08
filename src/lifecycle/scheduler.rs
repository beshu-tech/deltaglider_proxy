// SPDX-License-Identifier: BUSL-1.1

//! Conservative lifecycle scheduler.

use crate::api::handlers::AppState;
use crate::background::parse_duration_or;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::config_sections::{JobsConfig, LeaseTiming, LifecycleConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_TICK: Duration = Duration::from_secs(3600);
const MIN_TICK: Duration = Duration::from_secs(60);
// Lifecycle leases default to 5x maintenance's (`LeaseTiming::LIFECYCLE`,
// 300s/60s): lifecycle wakes at most once a minute (MIN_TICK) and typically
// hourly (DEFAULT_TICK), and one run does full prefix scans + deletes, so a
// longer TTL keeps the lease alive across a slow run without a peer stealing
// it mid-flight. The tradeoff: a crashed holder blocks a peer for up to one
// TTL. `advanced.jobs` overrides it.

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

            let (lifecycle, lease) = {
                let cfg = config.read().await;
                (cfg.lifecycle.clone(), lease_timing(&cfg.jobs))
            };
            if lifecycle.enabled {
                run_due_rules(&lifecycle, &config, lease, db.clone(), &state, &instance_id).await;
            } else {
                debug!("Lifecycle scheduler skipped: global lifecycle disabled");
            }
        }
    })
}

/// Whether `rule` (from the tick's snapshot) is still the live rule, under a
/// live lifecycle that is on. One rule of a tick can run for hours; the
/// operator can delete, disable or edit a later rule meanwhile.
async fn rule_is_live(live: &SharedConfig, rule: &crate::config_sections::LifecycleRule) -> bool {
    let cfg = live.read().await;
    cfg.lifecycle.enabled && super::planner::claimed_rule_is_current(rule, &cfg.lifecycle.rules)
}

async fn run_due_rules(
    lifecycle: &LifecycleConfig,
    live: &SharedConfig,
    lease: LeaseTiming,
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
        // Before the state row and the lease: a deleted rule must not get its
        // state row back.
        if !rule_is_live(live, rule).await {
            debug!(
                "Lifecycle scheduler skipping rule '{}': it changed after the tick started",
                rule.name
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
                        lease.ttl_secs,
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
        // Again under the lease: only the lease makes a delete refuse, so a
        // change between the first check and the lease is visible now.
        if !rule_is_live(live, rule).await {
            info!(
                "Lifecycle scheduler skipping rule '{}': it changed after the tick started",
                rule.name
            );
            if let Some(db) = db.as_ref() {
                let db = db.lock().await;
                let _ = db.lifecycle_release_lease(&rule.name, instance_id);
            }
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
            // Release the just-acquired lease — leaking it blocks run-now
            // for a full TTL after the maintenance window.
            if let Some(db) = db.as_ref() {
                let db = db.lock().await;
                let _ = db.lifecycle_release_lease(&rule.name, instance_id);
            }
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
                ttl_secs: lease.ttl_secs,
                heartbeat_secs: lease.heartbeat_secs,
            }),
            Some(state.maintenance_gate.clone()),
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

/// The lifecycle lease timing: `advanced.jobs`, else `LeaseTiming::LIFECYCLE`.
pub(crate) fn lease_timing(jobs: &JobsConfig) -> LeaseTiming {
    jobs.lease_timing(LeaseTiming::LIFECYCLE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DynStorageBackend;

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

    // ── The scheduler loop over a real engine (filesystem) + config DB ──

    struct Env {
        state: Arc<AppState>,
        db: Arc<Mutex<ConfigDb>>,
        _data: tempfile::TempDir,
    }

    async fn env() -> Env {
        let data = tempfile::tempdir().unwrap();
        let config = crate::config::Config::default();
        let backend: Box<crate::storage::DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(data.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        );
        let state = Arc::new(AppState {
            engine: arc_swap::ArcSwap::from_pointee(engine),
            multipart: Arc::new(crate::multipart::MultipartStore::new(
                config.max_object_size,
            )),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            usage_scanner: Arc::new(crate::usage_scanner::UsageScanner::new()),
            bucket_usage: None,
            reference_lock: None,
            config_db: None,
            maintenance_gate: Arc::new(crate::maintenance::gate::MaintenanceGate::new()),
            maintenance_notify: Arc::new(tokio::sync::Notify::new()),
            backend_capabilities: Default::default(),
            backend_health: Default::default(),
        });
        let db = Arc::new(Mutex::new(ConfigDb::in_memory("test").unwrap()));
        Env {
            state,
            db,
            _data: data,
        }
    }

    /// A lifecycle section with one rule per `(name, bucket)`, each deleting
    /// everything under `old/` older than 1 ms.
    fn lifecycle(rules: &[(&str, &str)]) -> LifecycleConfig {
        let mut yaml = String::from("enabled: true\ntick_interval: 1h\nrules:\n");
        for (name, bucket) in rules {
            yaml.push_str(&format!(
                "  - name: {name}\n    enabled: true\n    bucket: {bucket}\n    prefix: old/\n    \
                 action: delete\n    expire_after: 1ms\n"
            ));
        }
        serde_yaml::from_str(&yaml).unwrap()
    }

    async fn seed(env: &Env, bucket: &str) {
        let engine = env.state.engine.load();
        engine.create_bucket(bucket).await.unwrap();
        engine
            .store(bucket, "old/a.txt", b"expired", None, Default::default())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    async fn exists(env: &Env, bucket: &str) -> bool {
        env.state
            .engine
            .load()
            .head(bucket, "old/a.txt")
            .await
            .is_ok()
    }

    async fn run(env: &Env, cfg: &LifecycleConfig) {
        run_with_live(env, cfg, cfg).await;
    }

    /// One tick over the `snapshot`, while the live config holds `live`.
    async fn run_with_live(env: &Env, snapshot: &LifecycleConfig, live: &LifecycleConfig) {
        let live = crate::config::Config {
            lifecycle: live.clone(),
            ..Default::default()
        }
        .into_shared();
        run_due_rules(
            snapshot,
            &live,
            LeaseTiming::LIFECYCLE,
            Some(env.db.clone()),
            &env.state,
            "sched-test",
        )
        .await;
    }

    /// B008: a tick runs its rules one after another from a snapshot, and
    /// one rule can run for hours. A rule deleted, disabled or edited in that
    /// time, or lifecycle turned off, must not run from the snapshot.
    #[tokio::test]
    async fn a_rule_changed_after_the_snapshot_does_not_run() {
        let env = env().await;
        for (i, case) in ["deleted", "disabled", "edited", "lifecycle-off"]
            .iter()
            .enumerate()
        {
            let bucket = format!("sched-stale-{i}");
            seed(&env, &bucket).await;
            let snapshot = lifecycle(&[(&format!("sched-stale-rule-{i}"), &bucket)]);
            let mut live = snapshot.clone();
            match *case {
                "deleted" => live.rules.clear(),
                "disabled" => live.rules[0].enabled = false,
                "edited" => live.rules[0].expire_after = Some("30d".into()),
                _ => live.enabled = false,
            }
            run_with_live(&env, &snapshot, &live).await;
            assert!(
                exists(&env, &bucket).await,
                "a {case} rule ran from the tick's snapshot"
            );
        }
    }

    /// A due rule runs, releases its lease, and is not due again until one
    /// tick later.
    #[tokio::test]
    async fn a_due_rule_runs_then_waits_one_tick() {
        let env = env().await;
        seed(&env, "sched-due").await;
        let cfg = lifecycle(&[("sched-due-rule", "sched-due")]);
        run(&env, &cfg).await;
        assert!(
            !exists(&env, "sched-due").await,
            "the expired object is deleted"
        );
        let db = env.db.lock().await;
        let st = db.lifecycle_load_state("sched-due-rule").unwrap().unwrap();
        assert_eq!(st.last_status, "succeeded", "{st:?}");
        let now = crate::lifecycle::current_unix_seconds();
        assert!(
            st.next_due_at >= now + 3600 - 5,
            "the next run is one tick away: {st:?}"
        );
        assert!(
            db.lifecycle_try_acquire_lease("sched-due-rule", "other", now, 60)
                .unwrap(),
            "the scheduler released its lease"
        );
        db.lifecycle_release_lease("sched-due-rule", "other")
            .unwrap();
        drop(db);

        // Not due: a new expired object stays.
        seed(&env, "sched-due").await;
        run(&env, &cfg).await;
        assert!(
            exists(&env, "sched-due").await,
            "a rule that is not due does not run"
        );
    }

    #[tokio::test]
    async fn a_paused_rule_is_skipped() {
        let env = env().await;
        seed(&env, "sched-paused").await;
        {
            let db = env.db.lock().await;
            let now = crate::lifecycle::current_unix_seconds();
            db.lifecycle_ensure_state("sched-paused-rule", now).unwrap();
            db.lifecycle_set_paused("sched-paused-rule", true).unwrap();
        }
        run(&env, &lifecycle(&[("sched-paused-rule", "sched-paused")])).await;
        assert!(
            exists(&env, "sched-paused").await,
            "a paused rule deletes nothing"
        );
    }

    /// A rule whose bucket a maintenance job gates is deferred, and its
    /// lease is released so run-now is not blocked for a TTL.
    #[tokio::test]
    async fn a_maintenance_gated_bucket_defers_the_rule_and_frees_the_lease() {
        let env = env().await;
        seed(&env, "sched-gated").await;
        env.state.maintenance_gate.set_busy("sched-gated");
        let cfg = lifecycle(&[("sched-gated-rule", "sched-gated")]);
        run(&env, &cfg).await;
        assert!(
            exists(&env, "sched-gated").await,
            "deferred: nothing deleted"
        );
        {
            let db = env.db.lock().await;
            let now = crate::lifecycle::current_unix_seconds();
            assert!(
                db.lifecycle_try_acquire_lease("sched-gated-rule", "other", now, 60)
                    .unwrap(),
                "the deferred rule's lease is free"
            );
            db.lifecycle_release_lease("sched-gated-rule", "other")
                .unwrap();
            let st = db
                .lifecycle_load_state("sched-gated-rule")
                .unwrap()
                .unwrap();
            assert!(
                st.last_run_at.is_none_or(|t| t == 0),
                "no run recorded: {st:?}"
            );
        }
        env.state.maintenance_gate.clear("sched-gated");
        run(&env, &cfg).await;
        assert!(
            !exists(&env, "sched-gated").await,
            "runs once the gate clears"
        );
    }

    /// Another holder's live lease: the scheduler skips the rule.
    #[tokio::test]
    async fn a_rule_leased_elsewhere_is_skipped() {
        let env = env().await;
        seed(&env, "sched-leased").await;
        {
            let db = env.db.lock().await;
            let now = crate::lifecycle::current_unix_seconds();
            db.lifecycle_ensure_state("sched-leased-rule", now).unwrap();
            assert!(db
                .lifecycle_try_acquire_lease("sched-leased-rule", "peer", now, 300)
                .unwrap());
        }
        run(&env, &lifecycle(&[("sched-leased-rule", "sched-leased")])).await;
        assert!(exists(&env, "sched-leased").await);
    }

    /// Duplicate names and a rule that can never run are skipped; a valid
    /// rule next to them still runs.
    #[tokio::test]
    async fn duplicate_and_invalid_rules_are_skipped() {
        let env = env().await;
        seed(&env, "sched-dup").await;
        seed(&env, "sched-bad").await;
        seed(&env, "sched-ok").await;
        let mut cfg = lifecycle(&[
            ("sched-dup-rule", "sched-dup"),
            ("sched-dup-rule", "sched-dup"),
            ("sched-bad-rule", "sched-bad"),
            ("sched-ok-rule", "sched-ok"),
        ]);
        cfg.rules[2].expire_after = None;
        run(&env, &cfg).await;
        assert!(exists(&env, "sched-dup").await, "duplicated names: skipped");
        assert!(exists(&env, "sched-bad").await, "no expire_after: skipped");
        assert!(!exists(&env, "sched-ok").await, "the valid rule runs");
    }

    /// Without a config DB the in-process guard picks one runner: a rule
    /// already running here is skipped.
    #[tokio::test]
    async fn without_a_db_the_process_guard_serialises_a_rule() {
        let env = env().await;
        seed(&env, "sched-nodb").await;
        let cfg = lifecycle(&[("sched-nodb-rule", "sched-nodb")]);
        let live = crate::config::Config {
            lifecycle: cfg.clone(),
            ..Default::default()
        }
        .into_shared();
        let held = crate::lifecycle::try_acquire_rule("sched-nodb-rule").unwrap();
        run_due_rules(
            &cfg,
            &live,
            LeaseTiming::LIFECYCLE,
            None,
            &env.state,
            "sched-test",
        )
        .await;
        assert!(exists(&env, "sched-nodb").await, "busy here: skipped");
        drop(held);
        run_due_rules(
            &cfg,
            &live,
            LeaseTiming::LIFECYCLE,
            None,
            &env.state,
            "sched-test",
        )
        .await;
        assert!(!exists(&env, "sched-nodb").await, "free: runs");
    }

    /// The spawned loop sleeps one tick, then runs due rules; with lifecycle
    /// disabled it runs nothing. Virtual time: the minimum tick is 60 s.
    #[tokio::test(start_paused = true)]
    async fn the_spawned_scheduler_runs_rules_each_tick_while_enabled() {
        let env = env().await;
        seed(&env, "sched-loop").await;
        let mut disabled = lifecycle(&[("sched-loop-rule", "sched-loop")]);
        disabled.enabled = false;
        let config = crate::config::Config {
            lifecycle: disabled,
            ..Default::default()
        };
        let shared: crate::config::SharedConfig = Arc::new(tokio::sync::RwLock::new(config));
        let task = spawn_scheduler(shared.clone(), Some(env.db.clone()), env.state.clone());
        tokio::time::sleep(Duration::from_secs(3601)).await;
        assert!(exists(&env, "sched-loop").await, "disabled: no run");
        shared.write().await.lifecycle.enabled = true;
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while exists(&env, "sched-loop").await {
            assert!(
                std::time::Instant::now() < deadline,
                "the next tick runs the rule"
            );
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
        task.abort();
    }
}
