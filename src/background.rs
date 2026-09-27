// SPDX-License-Identifier: BUSL-1.1

//! Shared infrastructure for background job runners.
//!
//! - [`parse_duration_or`] — env-var duration parsing with sensible
//!   defaults (used by replication, lifecycle, event_delivery).
//! - [`RunLease`] — per-run leader-lease knobs, shared by the replication +
//!   lifecycle workers.
//! - [`LeaseKeeper`] — THE timer-driven lease renewal of every job kind
//!   (maintenance, lifecycle, replication, parity), with one error policy
//!   ([`crate::config_db::job_store::keeper_step`]).

use std::future::Future;
use std::time::Duration;
use tracing::warn;

use crate::config_db::job_store::{keeper_step, KeeperStep};
use crate::coordination::LeaseError;

/// Leader-lease knobs threaded through a leased background run.
#[derive(Debug, Clone)]
pub struct RunLease {
    pub owner: String,
    pub ttl_secs: i64,
    pub heartbeat_secs: i64,
}

pub(crate) fn parse_duration_or(
    value: &str,
    default: Duration,
    minimum: Duration,
    label: &str,
) -> Duration {
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

/// Renews a lease on a timer for as long as it lives (aborted on drop). A
/// run checks [`LeaseAlive`] at its stop points: after the keeper gives the
/// lease up, the run must not start more work.
pub(crate) struct LeaseKeeper {
    task: tokio::task::JoinHandle<()>,
    alive: LeaseAlive,
}

/// The keeper's verdict, readable from the run (cheap to clone).
#[derive(Clone)]
pub(crate) struct LeaseAlive(tokio::sync::watch::Receiver<bool>);

impl LeaseAlive {
    /// A verdict that stays alive (no lease to lose).
    pub(crate) fn always() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(true);
        // The value never changes once the sender is gone.
        drop(tx);
        Self(rx)
    }

    pub(crate) fn is_alive(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the lease is lost; never while it is held.
    pub(crate) async fn lost(&self) {
        let mut rx = self.0.clone();
        if rx.wait_for(|alive| !*alive).await.is_err() {
            // Keeper gone with the lease still held: nothing more to report.
            std::future::pending::<()>().await;
        }
    }
}

impl LeaseKeeper {
    /// Renew every `interval` through `renew`; judge each result with
    /// [`keeper_step`] against `ttl`. `label` names the lease in the logs.
    pub(crate) fn spawn<F, Fut>(
        label: String,
        interval: Duration,
        ttl: Duration,
        mut renew: F,
    ) -> Self
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), LeaseError>> + Send,
    {
        let (tx, rx) = tokio::sync::watch::channel(true);
        let task = tokio::spawn(async move {
            let mut last_ok = tokio::time::Instant::now();
            loop {
                tokio::time::sleep(interval).await;
                let renewed = renew().await;
                match keeper_step(&renewed, last_ok.elapsed(), interval, ttl) {
                    KeeperStep::Held => last_ok = tokio::time::Instant::now(),
                    KeeperStep::Retry => warn!(
                        "{label}: lease renewal failed ({}); retrying",
                        renewed.err().map(|e| e.to_string()).unwrap_or_default()
                    ),
                    KeeperStep::Lost => {
                        let _ = tx.send(false);
                        warn!(
                            "{label}: lease lost ({}); the run stops before more work",
                            renewed.err().map(|e| e.to_string()).unwrap_or_default()
                        );
                        return;
                    }
                }
            }
        });
        Self {
            task,
            alive: LeaseAlive(rx),
        }
    }

    /// The verdict handle a run carries to its stop points.
    pub(crate) fn alive(&self) -> LeaseAlive {
        self.alive.clone()
    }
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod keeper_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A keeper whose renewals answer from a script (then repeat the last).
    fn scripted(
        script: Vec<Result<(), LeaseError>>,
        ttl_ms: u64,
    ) -> (LeaseKeeper, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let keeper = LeaseKeeper::spawn(
            "test".into(),
            Duration::from_millis(100),
            Duration::from_millis(ttl_ms),
            move || {
                let i = c.fetch_add(1, Ordering::SeqCst);
                let r = script[i.min(script.len() - 1)].clone();
                async move { r }
            },
        );
        (keeper, calls)
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_is_lost_at_once() {
        let (k, _) = scripted(vec![Err(LeaseError::Lost)], 10_000);
        let alive = k.alive();
        tokio::time::timeout(Duration::from_secs(1), alive.lost())
            .await
            .expect("lost after the first refusal");
        assert!(!alive.is_alive());
    }

    /// A store error is retried: one failed renewal must not stop a run.
    #[tokio::test(start_paused = true)]
    async fn a_store_error_is_retried_and_a_later_renewal_holds() {
        let busy = Err(LeaseError::Backend("db locked".into()));
        let (k, calls) = scripted(vec![busy.clone(), busy, Ok(())], 10_000);
        tokio::time::sleep(Duration::from_millis(1_050)).await;
        assert!(calls.load(Ordering::SeqCst) >= 5);
        assert!(k.alive().is_alive());
    }

    /// Errors until the lease would expire: lost, so the run does not go on
    /// under a lapsed lease.
    #[tokio::test(start_paused = true)]
    async fn errors_until_the_ttl_are_lost() {
        let (k, _) = scripted(vec![Err(LeaseError::Backend("down".into()))], 450);
        let alive = k.alive();
        tokio::time::timeout(Duration::from_secs(2), alive.lost())
            .await
            .expect("lost before the ttl");
        assert!(!alive.is_alive());
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_keeper_stops_renewing() {
        let (k, calls) = scripted(vec![Ok(())], 10_000);
        tokio::time::sleep(Duration::from_millis(350)).await;
        drop(k);
        let before = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }

    #[test]
    fn always_stays_alive() {
        assert!(LeaseAlive::always().is_alive());
    }
}
