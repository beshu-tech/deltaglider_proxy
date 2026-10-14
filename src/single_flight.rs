// SPDX-License-Identifier: BUSL-1.1

//! One shared run of an async computation per key ("single flight").
//!
//! Callers that ask for the same key while a run is in progress wait for
//! that run and get a clone of its answer, an error answer included. The
//! answer is not kept after the flight: the next caller after the flight
//! ends starts a new run. A caller that is cancelled (its future dropped)
//! hands the run to the next waiter, and the last participant to leave
//! removes the flight, so a cancelled request leaves nothing behind.
//!
//! THE single-flight primitive: the cold bucket check of the S3 write paths
//! (`api::handlers::ensure_bucket_exists`), the routing resolver's bucket
//! probe (`storage::routing`) and the reference cache's load
//! (`deltaglider::cache`) use it. The source guard
//! `single_flights_use_the_shared_primitive` refuses a hand-rolled one.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

type Cell<V> = Arc<tokio::sync::OnceCell<V>>;

/// The flights in progress, by key.
pub(crate) struct SingleFlight<K, V> {
    flights: parking_lot::Mutex<HashMap<K, Cell<V>>>,
}

impl<K, V> Default for SingleFlight<K, V> {
    fn default() -> Self {
        Self {
            flights: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone, V: Clone> SingleFlight<K, V> {
    /// The answer of the flight of `key`: the running one, else a new one
    /// that runs `work`. `work` runs only when this caller is the one that
    /// does the flight's run.
    pub(crate) async fn run<F, Fut>(&self, key: K, work: F) -> V
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = V>,
    {
        let cell = self.flights.lock().entry(key.clone()).or_default().clone();
        let leave = Leave {
            flights: &self.flights,
            key,
            cell,
        };
        leave.cell.get_or_init(work).await.clone()
    }

    /// No flight is in progress (tests).
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        self.flights.lock().is_empty()
    }

    /// The number of flights in progress whose key matches `f` (tests).
    #[cfg(test)]
    pub(crate) fn count(&self, f: impl Fn(&K) -> bool) -> usize {
        self.flights.lock().keys().filter(|k| f(k)).count()
    }
}

/// One participant of a flight. When it leaves (answered, or its future
/// dropped), it removes the flight if the flight answered, or if no other
/// participant is left. A flight whose run was cancelled stays while other
/// participants wait: the next of them does the run.
struct Leave<'a, K: Eq + Hash, V> {
    flights: &'a parking_lot::Mutex<HashMap<K, Cell<V>>>,
    key: K,
    cell: Cell<V>,
}

impl<K: Eq + Hash, V> Drop for Leave<'_, K, V> {
    fn drop(&mut self) {
        let mut flights = self.flights.lock();
        let Some(current) = flights.get(&self.key) else {
            return;
        };
        if !Arc::ptr_eq(current, &self.cell) {
            return;
        }
        // Participants clone the cell under this lock, so the count cannot
        // grow behind this check: 2 = the map and this participant.
        if self.cell.initialized() || Arc::strong_count(&self.cell) == 2 {
            flights.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    /// N concurrent callers share one run, an error answer included.
    #[tokio::test]
    async fn concurrent_callers_share_one_run_and_its_error() {
        let flights: SingleFlight<&str, Result<u32, String>> = SingleFlight::default();
        let runs = AtomicUsize::new(0);
        let work = || async {
            runs.fetch_add(1, SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<u32, _>("down".to_string())
        };
        let all = futures::future::join_all((0..8).map(|_| flights.run("k", work))).await;
        assert!(all.iter().all(|r| r == &Err("down".to_string())));
        assert_eq!(runs.load(SeqCst), 1);
        assert!(flights.is_idle());
    }

    /// The answer is not kept after the flight: the next caller runs again.
    #[tokio::test]
    async fn an_answer_is_not_kept_after_the_flight() {
        let flights: SingleFlight<&str, Result<u32, String>> = SingleFlight::default();
        let runs = AtomicUsize::new(0);
        for _ in 0..2 {
            let r = flights
                .run("k", || async {
                    runs.fetch_add(1, SeqCst);
                    Err::<u32, _>("down".to_string())
                })
                .await;
            assert!(r.is_err());
        }
        assert_eq!(runs.load(SeqCst), 2);
    }

    /// A cancelled caller leaves no flight behind; a cancelled runner hands
    /// the run to the next waiter, which still shares one flight.
    #[tokio::test]
    async fn a_cancelled_caller_leaves_no_flight() {
        let flights: SingleFlight<u32, u32> = SingleFlight::default();
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            7
        };
        for key in 0..5 {
            let _ = tokio::time::timeout(Duration::from_millis(20), flights.run(key, slow)).await;
        }
        assert!(flights.is_idle(), "every cancelled flight is gone");

        // The runner is cancelled while a second caller waits: the waiter
        // does the run, and the flight is gone when it ends.
        let runs = AtomicUsize::new(0);
        let work = || async {
            runs.fetch_add(1, SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            9
        };
        let first = tokio::time::timeout(Duration::from_millis(20), flights.run(1, work));
        let second = async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            flights.run(1, work).await
        };
        let (first, second) = tokio::join!(first, second);
        assert!(first.is_err(), "the first caller timed out");
        assert_eq!(second, 9);
        assert_eq!(runs.load(SeqCst), 2, "the waiter took over the run");
        assert!(flights.is_idle());
    }

    /// A storage error that one flight hands to each waiter keeps its
    /// variant, its text and an I/O error's OS code, so each waiter maps
    /// it to the same S3 answer.
    #[test]
    fn a_shared_storage_error_keeps_its_variant() {
        use crate::storage::StorageError;
        let e = StorageError::Throttled("slow".into()).duplicate();
        assert!(matches!(&e, StorageError::Throttled(m) if m == "slow"));
        let io = StorageError::Io(std::io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        let StorageError::Io(copy) = io.duplicate() else {
            panic!("not Io");
        };
        assert_eq!(copy.raw_os_error(), Some(libc::ENAMETOOLONG));
        assert_eq!(io.to_string(), StorageError::Io(copy).to_string());
    }

    /// Source guard: a single flight is this primitive, not a hand-rolled
    /// map of `OnceCell`s or of async mutexes. A hand-rolled one forgot to
    /// share a failed answer and to remove a cancelled caller's entry
    /// (review 2d: the bucket check, the routing probe, then the reference
    /// cache, a map of `tokio::sync::Mutex<()>`). The lifecycle preview
    /// cells are a cache with a TTL (they keep their answer past the flight
    /// by design), not a single flight. The deltaspace lock table
    /// (`engine/locking.rs`) is a lock, not a single flight: each holder
    /// does its own work under it.
    #[test]
    fn single_flights_use_the_shared_primitive() {
        const ONCE_CELL_ALLOWED: &[&str] = &["src/single_flight.rs", "src/lifecycle/worker.rs"];
        const MUTEX_MAP_ALLOWED: &[&str] = &["src/deltaglider/engine/locking.rs"];
        let mut offenders = Vec::new();
        for (rel, text) in crate::source_scan::prod_sources("src") {
            for (n, line) in crate::source_scan::prod_lines(&text) {
                let once_cell = line.contains("OnceCell<")
                    && !line.contains("OnceLock")
                    && !ONCE_CELL_ALLOWED.contains(&rel.as_str());
                let mutex_map = line.contains("Map<")
                    && line.contains("Mutex<()>")
                    && !MUTEX_MAP_ALLOWED.contains(&rel.as_str());
                if once_cell || mutex_map {
                    offenders.push(format!("{rel}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use crate::single_flight::SingleFlight:\n{}",
            offenders.join("\n")
        );
    }
}
