// SPDX-License-Identifier: BUSL-1.1

//! Config mutation seam for BACKGROUND tasks.
//!
//! The admin API mutates config inside its handlers (where it has
//! `AdminState`); background workers — today, migrate jobs that stage a
//! transient route and flip a bucket's backend — need the same
//! "mutate → rebuild engine → persist" transaction without `AdminState`.
//! Verified: the engine rebuild needs only `AppState.engine` +
//! `AppState.metrics`, so this seam carries `Arc<AppState>` and the
//! resolved config-file path (the SAME path the admin API persists to —
//! resolved once in main and shared, never re-derived, so a worker can't
//! write to a different file than the operator's apply does).
//!
//! The mutation runs through the SAME transition as every admin write
//! (`crate::api::admin::config::apply_config_transition`) in its
//! background scope: the gates and the engine rebuild run, the steps that
//! need admin-side state are skipped, and a change that needs one of them
//! is refused (see `TransitionCtx::Background`).

use std::sync::Arc;

use crate::api::handlers::AppState;
use crate::config::{Config, SharedConfig};
use crate::deltaglider::DynEngine;

/// Build an engine from `cfg` WITHOUT installing it. The admin transition
/// builds in its pre-commit phase and installs last (review 4 config-1).
pub async fn build_engine(app: &AppState, cfg: &Config) -> Result<DynEngine, String> {
    let new_engine = DynEngine::new(cfg, Some(app.metrics.clone()))
        .await
        .map_err(|e| e.to_string())?;
    // Re-attach the usage counter and cross-instance reference lock — a
    // rebuild must not drop either.
    Ok(new_engine
        .with_bucket_usage(app.bucket_usage.clone())
        .with_reference_lock(app.reference_lock.clone()))
}

/// Drop the runtime rows (state, cursor, lease, run history, failures,
/// parity) of every replication and lifecycle rule that `cfg` does not
/// name. A rule removed or renamed in the YAML (an apply, IaC) kept its
/// rows forever: only a rule delete through the admin API dropped them.
/// Runs after every config apply and at boot. Never fails the caller: a
/// failed purge leaves rows that the next apply or boot drops.
pub fn reconcile_job_rules(db: &crate::config_db::JobsDb, cfg: &Config) {
    let replication: Vec<String> = cfg
        .replication
        .rules
        .iter()
        .map(|r| r.name.clone())
        .collect();
    let lifecycle: Vec<String> = cfg.lifecycle.rules.iter().map(|r| r.name.clone()).collect();
    for (what, result) in [
        ("replication", db.replication_reconcile_rules(&replication)),
        ("lifecycle", db.lifecycle_reconcile_rules(&lifecycle)),
    ] {
        match result {
            Ok(0) => {}
            Ok(n) => tracing::info!("dropped the runtime rows of {n} removed {what} rule(s)"),
            Err(e) => tracing::warn!("could not drop the rows of removed {what} rules: {e}"),
        }
    }
}

/// Hot-swap an engine built by [`build_engine`] into `app.engine`.
pub fn install_engine(app: &AppState, engine: DynEngine, context: &str) {
    app.engine.store(Arc::new(engine));
    tracing::info!("{}", context);
}

#[derive(Clone)]
pub struct ConfigMutator {
    pub config: SharedConfig,
    pub app: Arc<AppState>,
    /// The config file every successful mutation persists to. Resolved
    /// once in main; identical to the admin API's persistence target.
    pub persist_path: String,
}

impl ConfigMutator {
    /// Write-lock the config, apply `mutate`, rebuild the engine, persist.
    ///
    /// Rollback contract: the mutation is built on a clone; if the
    /// transition refuses it (a gate, the engine build, a change only the
    /// admin API may make), nothing is swapped and the error is returned.
    /// A persist failure after a successful rebuild is warn-only: the
    /// running state is correct and a later persist (any admin apply)
    /// writes the same content.
    pub async fn mutate_and_apply(
        &self,
        context: &str,
        mutate: impl FnOnce(&mut Config),
    ) -> Result<(), String> {
        self.mutate_inner(context, mutate, false).await
    }

    /// Like [`mutate_and_apply`] but a PERSIST failure is a hard error and
    /// the whole mutation is rolled back (config restored, engine rebuilt
    /// back). Use for mutations where the FILE lagging the running state
    /// is dangerous — the migrate FLIP is the canonical case: if the file
    /// still routed the bucket to the source after a crash, the resumed
    /// cleanup phase would delete live data off a bucket clients are
    /// actively writing to.
    ///
    /// [`mutate_and_apply`]: Self::mutate_and_apply
    pub async fn mutate_and_apply_strict(
        &self,
        context: &str,
        mutate: impl FnOnce(&mut Config),
    ) -> Result<(), String> {
        self.mutate_inner(context, mutate, true).await
    }

    async fn mutate_inner(
        &self,
        context: &str,
        mutate: impl FnOnce(&mut Config),
        persist_required: bool,
    ) -> Result<(), String> {
        use crate::api::admin::{apply_config_transition, TransitionCtx};
        let ctx = || TransitionCtx::Background {
            app: &self.app,
            context,
        };
        let mut cfg = self.config.write().await;
        let rollback = cfg.clone();
        // The engine that serves `rollback`: put back as is if the engine
        // rollback below cannot rebuild it.
        let rollback_engine = self.app.engine.load_full();
        let mut new_cfg = rollback.clone();
        mutate(&mut new_cfg);
        // On Err nothing changed: the old config and engine keep serving.
        apply_config_transition(ctx(), &mut cfg, new_cfg)
            .await
            .map_err(|e| format!("engine rebuild failed ({context}): {e}"))?;
        if let Err(e) = cfg.persist_to_file(&self.persist_path) {
            if persist_required {
                // Unwind fully: transition back to the old config AND engine
                // so memory and file agree again.
                let back = TransitionCtx::Background {
                    app: &self.app,
                    context: "rollback after failed persist",
                };
                let restore_err = apply_config_transition(back, &mut cfg, rollback.clone())
                    .await
                    .err();
                if restore_err.is_some() {
                    // The file still holds the old config: memory follows
                    // it, and so must the engine (the new one routes the
                    // way the file does not).
                    *cfg = rollback;
                    self.app.engine.store(rollback_engine);
                }
                return Err(format!(
                    "config persist to '{}' failed ({context}): {e}{}",
                    self.persist_path,
                    restore_err
                        .map(|r| format!(" (and engine rollback also failed: {r})"))
                        .unwrap_or_default()
                ));
            }
            tracing::warn!(
                "config persist to '{}' failed after '{}': {} — running state is \
                 correct; the next successful persist writes the same content",
                self.persist_path,
                context,
                e
            );
        }
        if let Some(db) = &self.app.jobs_db {
            // Not under the config write lock: a task that holds the DB
            // mutex may wait for the config.
            let live = cfg.clone();
            drop(cfg);
            reconcile_job_rules(&*db.lock().await, &live);
        }
        Ok(())
    }

    /// Read-lock the live config.
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, Config> {
        self.config.read().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// jobs-12: a rule removed from the YAML kept its rows (state, run
    /// history, failures) forever: only a rule delete through the admin
    /// API dropped them. An apply without the rule drops them.
    #[tokio::test]
    async fn an_apply_drops_the_rows_of_removed_rules() {
        let data = tempfile::tempdir().unwrap();
        let yaml = format!(
            "storage:\n  backends:\n  - name: local\n    type: filesystem\n    path: {}\n  default_backend: local\n  lifecycle:\n    rules:\n    - name: keep\n      bucket: b\n      expire_after: 30d\n    - name: old\n      bucket: b\n      expire_after: 30d\n",
            data.path().display()
        );
        let cfg = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(cfg.lifecycle.rules.len(), 2, "{:?}", cfg.lifecycle);
        let db = crate::config_db::JobsDb::in_memory("k").unwrap();
        for rule in ["keep", "old"] {
            db.lifecycle_ensure_state(rule, 1).unwrap();
            let run = db.lifecycle_begin_run(rule, 1, "test").unwrap();
            db.lifecycle_record_failure(
                rule,
                crate::lifecycle::state_store::LifecycleFailureInsert {
                    run_id: Some(run),
                    occurred_at: 1,
                    bucket: "b",
                    object_key: "k",
                    error_message: "boom",
                },
                10,
            )
            .unwrap();
        }
        db.replication_ensure_state("gone-repl", 1).unwrap();
        let engine = DynEngine::new(&cfg, None).await.unwrap();
        let app = Arc::new(AppState {
            jobs_db: Some(Arc::new(tokio::sync::Mutex::new(db))),
            ..Arc::into_inner(AppState::for_tests(engine)).unwrap()
        });
        let mutator = ConfigMutator {
            config: cfg.into_shared(),
            app: app.clone(),
            persist_path: data.path().join("c.yaml").display().to_string(),
        };
        mutator
            .mutate_and_apply("test", |c| c.lifecycle.rules.retain(|r| r.name != "old"))
            .await
            .unwrap();
        let db = app.jobs_db.as_ref().unwrap().lock().await;
        assert!(db.lifecycle_load_state("old").unwrap().is_none());
        assert!(db.lifecycle_recent_runs("old", 10).unwrap().is_empty());
        assert!(db.lifecycle_recent_failures("old", 10).unwrap().is_empty());
        assert!(db.lifecycle_load_state("keep").unwrap().is_some());
        assert_eq!(db.lifecycle_recent_failures("keep", 10).unwrap().len(), 1);
        assert!(db.replication_load_state("gone-repl").unwrap().is_none());
    }

    fn app_with(engine: DynEngine) -> Arc<AppState> {
        AppState::for_tests(engine)
    }

    /// B085: a strict mutation whose persist fails and whose engine
    /// rollback also fails must leave the config and the serving engine on
    /// the SAME routing (the file's): the config went back while the
    /// engine kept routing the bucket to the target.
    #[tokio::test]
    async fn a_failed_strict_rollback_keeps_config_and_engine_in_step() {
        let (src, dst, spare, ro) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let yaml = format!(
            "storage:\n  backends:\n  - name: src\n    type: filesystem\n    path: {}\n  - name: dst\n    type: filesystem\n    path: {}\n  - name: spare\n    type: filesystem\n    path: {}\n  default_backend: src\n  buckets:\n    b:\n      backend: src\n",
            src.path().display(),
            dst.path().display(),
            spare.path().display()
        );
        let cfg = Config::from_yaml_str(&yaml).unwrap();
        let engine = DynEngine::new(&cfg, None).await.unwrap();
        let app = app_with(engine);
        let mutator = ConfigMutator {
            config: cfg.into_shared(),
            app: app.clone(),
            persist_path: ro.path().join("c.yaml").display().to_string(),
        };
        // The persist fails (read-only dir), and so does a rebuild that
        // needs `spare` (read-only root).
        std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        std::fs::set_permissions(spare.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(ro.path().join("probe"), b"x").is_ok() {
            return; // root ignores mode bits
        }
        let result = mutator
            .mutate_and_apply_strict("test flip", |c| {
                c.buckets.get_mut("b").unwrap().backend = Some("dst".into());
                c.backends.retain(|b| b.name != "spare");
            })
            .await;
        std::fs::set_permissions(ro.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(spare.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err(), "the persist was meant to fail");
        let cfg_backend = mutator.read().await.buckets["b"].backend.clone();
        use crate::storage::StorageBackend;
        let engine_identity = app.engine.load().storage().storage_identity("b");
        assert!(
            engine_identity.starts_with(cfg_backend.as_deref().unwrap_or("?")),
            "config routes b to {cfg_backend:?}, the engine to {engine_identity:?}"
        );
    }
}
