// SPDX-License-Identifier: BUSL-1.1

//! The backend health gate, for replication jobs.
//!
//! A client request to a bucket whose backend is gated (unreachable, or its
//! credentials refused) gets a fast `503` (`coordination::health`). The jobs
//! read the same verdicts: a rule whose source or destination backend is
//! gated defers, as for a maintenance job on its destination, and sends
//! that backend nothing. Their full volume against a gated backend kept its
//! probe failing, and so the gate closed, for every client.

use crate::config::Config;
use crate::config_sections::ReplicationRule;
use crate::coordination::BackendHealthCache;
use crate::deltaglider::DynEngine;
use crate::storage::StorageBackend;

/// Why no job may send requests to `bucket` now: its backend is gated.
/// `None`: the backend is open, or was never probed (only a definitive
/// verdict gates, as for client requests).
pub(crate) fn gated_bucket(
    health: &BackendHealthCache,
    config: &Config,
    engine: &DynEngine,
    bucket: &str,
) -> Option<String> {
    // Fast path: nothing gated, no resolution.
    if health.unhealthy_names().is_empty() {
        return None;
    }
    let routed = engine.storage().resolved_backend_name(bucket);
    let (name, def) = crate::coordination::health::gated_backend(config, bucket, routed)?;
    let verdict = health.get(&name, &def)?;
    verdict.is_gating().then(|| {
        format!(
            "bucket '{bucket}' is on backend '{name}', which is unavailable: {}",
            verdict.cause()
        )
    })
}

/// Why `rule` must defer now: the backend of its source or its destination
/// is gated.
pub(crate) fn gated_rule(
    health: &BackendHealthCache,
    config: &Config,
    engine: &DynEngine,
    rule: &ReplicationRule,
) -> Option<String> {
    gated_bucket(health, config, engine, &rule.source.bucket)
        .or_else(|| gated_bucket(health, config, engine, &rule.destination.bucket))
}

/// The health gate a run re-checks at its control boundaries.
#[derive(Clone)]
pub struct RunHealthGate {
    pub health: std::sync::Arc<BackendHealthCache>,
    pub config: crate::config::SharedConfig,
}

impl RunHealthGate {
    /// [`gated_rule`], with the live config.
    pub(crate) async fn gated(&self, engine: &DynEngine, rule: &ReplicationRule) -> Option<String> {
        gated_rule(&self.health, &*self.config.read().await, engine, rule)
    }
}
