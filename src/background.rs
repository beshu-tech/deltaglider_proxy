// SPDX-License-Identifier: BUSL-1.1

//! Shared infrastructure for background job runners.
//!
//! - `parse_duration_or` — env-var duration parsing with sensible
//!   defaults (used by replication, lifecycle, event_delivery).
//! - `RunLease` — per-run leader-lease knobs, shared by the replication +
//!   lifecycle workers.
//! - `LeaseKeeper` — THE timer-driven lease renewal of every job kind
//!   (maintenance, lifecycle, replication, parity), with one error policy
//!   (`config_db::job_store::keeper_step`).
//! - [`spawn_supervised`] — THE spawn of a long-lived background loop: a
//!   panic is logged, counted, and the loop restarts after a backoff.

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

/// Panics of supervised background tasks, by task name. Registered by
/// `Metrics::new()` (a static: the supervisor has no Metrics handle).
pub static BACKGROUND_TASK_PANICS: std::sync::LazyLock<prometheus::IntCounterVec> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "deltaglider_background_task_panics_total",
                "Panics of supervised background tasks; each one restarts the task",
            ),
            &["task"],
        )
        .expect("valid metric")
    });

/// Restart delay of a supervised task: doubles per panic up to `max`, and
/// goes back to `initial` after a run that lasted at least `max`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RestartBackoff {
    pub initial: Duration,
    pub max: Duration,
}

impl RestartBackoff {
    pub(crate) const DEFAULT: Self = Self {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(60),
    };
}

/// Spawn a long-lived background task under supervision. `factory` builds
/// the task's future; a panic in it is logged with `name`, counted in
/// [`BACKGROUND_TASK_PANICS`], and the task is built again after a capped
/// exponential backoff (1 s to 60 s). A task that returns is done and is
/// not restarted. Nothing restarts once shutdown began. Aborting the
/// returned handle drops the running task.
pub fn spawn_supervised<F, Fut>(name: &'static str, factory: F) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    spawn_supervised_with(name, RestartBackoff::DEFAULT, factory)
}

pub(crate) fn spawn_supervised_with<F, Fut>(
    name: &'static str,
    backoff: RestartBackoff,
    mut factory: F,
) -> tokio::task::JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    use futures::FutureExt;
    tokio::spawn(async move {
        let mut delay = backoff.initial;
        loop {
            let started = tokio::time::Instant::now();
            // The factory call runs inside `catch_unwind` too.
            let run = std::panic::AssertUnwindSafe(async { factory().await })
                .catch_unwind()
                .await;
            let Err(payload) = run else { return };
            BACKGROUND_TASK_PANICS.with_label_values(&[name]).inc();
            let msg = panic_message(payload.as_ref());
            if crate::shutdown::is_shutting_down() {
                tracing::error!(
                    task = name,
                    "background task panicked during shutdown: {msg}"
                );
                return;
            }
            if started.elapsed() >= backoff.max {
                delay = backoff.initial;
            }
            tracing::error!(
                task = name,
                "background task panicked: {msg}; restarting in {}",
                humantime::format_duration(delay)
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = crate::shutdown::started() => return,
            }
            delay = (delay * 2).min(backoff.max);
        }
    })
}

/// The text of a panic payload (`panic!` gives a `&str` or a `String`).
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Route every panic through `tracing` (log shippers see it), then run the
/// default hook (stderr output and backtrace stay as they were).
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("<unnamed>"),
            location = %location,
            "panic: {}",
            panic_message(info.payload())
        );
        default(info);
    }));
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

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const FAST: RestartBackoff = RestartBackoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(40),
    };

    #[tokio::test]
    async fn a_panicking_task_restarts_and_is_counted() {
        const NAME: &str = "test-panics-then-returns";
        let before = BACKGROUND_TASK_PANICS.with_label_values(&[NAME]).get();
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let task = spawn_supervised_with(NAME, FAST, move || {
            let n = r.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    panic!("run {n} blows up");
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the third run returns, so the supervisor ends")
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(
            BACKGROUND_TASK_PANICS.with_label_values(&[NAME]).get() - before,
            2
        );
    }

    #[tokio::test]
    async fn a_panic_in_the_factory_restarts_too() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let task = spawn_supervised_with("test-factory-panics", FAST, move || {
            if r.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("factory blows up");
            }
            async {}
        });
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_task_that_returns_is_not_restarted() {
        const NAME: &str = "test-returns";
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let task = spawn_supervised_with(NAME, FAST, move || {
            r.fetch_add(1, Ordering::SeqCst);
            async {}
        });
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(BACKGROUND_TASK_PANICS.with_label_values(&[NAME]).get(), 0);
    }

    #[tokio::test]
    async fn aborting_the_handle_stops_the_running_task() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let task = spawn_supervised_with("test-abort", FAST, move || {
            let t = t.clone();
            async move {
                loop {
                    t.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ticks.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        task.abort();
        let _ = task.await;
        let after = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), after);
    }

    #[test]
    fn panic_message_reads_both_payload_kinds() {
        let p: Box<dyn std::any::Any + Send> = Box::new("static");
        assert_eq!(panic_message(p.as_ref()), "static");
        let p: Box<dyn std::any::Any + Send> = Box::new(String::from("owned"));
        assert_eq!(panic_message(p.as_ref()), "owned");
        let p: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(p.as_ref()), "non-string panic payload");
    }

    /// Long-lived loops of the binary go through [`spawn_supervised`]: a bare
    /// `tokio::spawn` there dies for good on its first panic.
    #[test]
    fn the_binary_spawns_background_loops_supervised() {
        // (file, the line after the spawn, why it may stay bare)
        const EXEMPT: &[(&str, &str, &str)] = &[
            (
                "src/main.rs",
                "shutdown_signal().await;",
                "one-shot: waits for the signal, then starts the TLS drain",
            ),
            (
                "src/startup.rs",
                "let engine = state.engine.load_full();",
                "one-shot: the boot-time plaintext sample returns after one pass",
            ),
        ];
        let mut offenders = Vec::new();
        for file in ["src/main.rs", "src/startup.rs"] {
            let text = crate::source_scan::read(file);
            let lines = crate::source_scan::prod_lines(&text);
            for (i, (n, line)) in lines.iter().enumerate() {
                if !line.contains("tokio::spawn(") {
                    continue;
                }
                let next = lines.get(i + 1).map(|(_, l)| l.trim()).unwrap_or("");
                let exempt = EXEMPT
                    .iter()
                    .any(|(f, frag, _)| *f == file && next == *frag);
                if !exempt {
                    offenders.push(format!("{file}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "spawn long-lived loops with background::spawn_supervised, or exempt a \
             one-shot task here with its reason:\n{}",
            offenders.join("\n")
        );
    }
}
