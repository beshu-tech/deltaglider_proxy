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

    fn app_with(engine: DynEngine) -> Arc<AppState> {
        let config = Config::default();
        Arc::new(AppState {
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
        })
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
