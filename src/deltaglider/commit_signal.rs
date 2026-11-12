// SPDX-License-Identifier: BUSL-1.1

//! The moment an object write commits, as a signal for the caller of a
//! store. The engine notes it where it records the commit
//! (`record_commit`, right after the backend write, before any other
//! await); a caller that wraps its store in [`watching`] learns that the
//! write is durable even when a later step errors or panics. A task that
//! the store spawns on its own does not inherit the watch (task-locals do
//! not cross `tokio::spawn`); the stores that use it run in one task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

tokio::task_local! {
    static COMMITTED: Arc<AtomicBool>;
}

/// Run `fut`; `flag` becomes true when a write inside it commits.
pub async fn watching<F: std::future::Future>(flag: Arc<AtomicBool>, fut: F) -> F::Output {
    COMMITTED.scope(flag, fut).await
}

/// The engine: a write just committed.
pub(crate) fn note() {
    let _ = COMMITTED.try_with(|flag| flag.store(true, Ordering::SeqCst));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_note_inside_a_watch_sets_the_flag_and_outside_sets_nothing() {
        let flag = Arc::new(AtomicBool::new(false));
        note();
        assert!(!flag.load(Ordering::SeqCst));
        watching(flag.clone(), async { note() }).await;
        assert!(flag.load(Ordering::SeqCst));
    }
}
