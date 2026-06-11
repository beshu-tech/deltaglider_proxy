// SPDX-License-Identifier: GPL-3.0-only

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
//! NOTE: `mutate_and_apply` deliberately does NOT rebuild the
//! bucket-derived snapshots (public-prefix / admission). That matches the
//! synchronous migrate flow it replaces; `__dgmigrate_*` transient
//! policies carry no public prefixes or admission rules, and the final
//! flip only changes `backend` routing — none of which feed those
//! snapshots.

use std::sync::Arc;

use crate::api::handlers::AppState;
use crate::config::{Config, SharedConfig};
use crate::deltaglider::DynEngine;

/// Rebuild the engine from `cfg` and hot-swap it into `app.engine`.
/// On failure the OLD engine keeps serving (nothing is swapped).
pub async fn rebuild_engine_only(
    app: &AppState,
    cfg: &Config,
    context: &str,
) -> Result<(), String> {
    match DynEngine::new(cfg, Some(app.metrics.clone())).await {
        Ok(new_engine) => {
            app.engine.store(Arc::new(new_engine));
            tracing::info!("{}", context);
            Ok(())
        }
        Err(e) => Err(format!("{}", e)),
    }
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
    /// Rollback contract: the pre-mutation config is cloned first; if the
    /// engine rebuild fails, the clone is restored (no rebuild needed —
    /// the OLD engine was never swapped out) and the error returned.
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
        let mut cfg = self.config.write().await;
        let rollback = cfg.clone();
        mutate(&mut cfg);
        if let Err(e) = rebuild_engine_only(&self.app, &cfg, context).await {
            *cfg = rollback;
            return Err(format!("engine rebuild failed ({context}): {e}"));
        }
        if let Err(e) = cfg.persist_to_file(&self.persist_path) {
            if persist_required {
                // Unwind fully: restore the old config AND swap the old
                // engine back so memory and file agree again.
                let restore_err = rebuild_engine_only(
                    &self.app,
                    &rollback,
                    &format!("rollback after failed persist ({context})"),
                )
                .await
                .err();
                *cfg = rollback;
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
