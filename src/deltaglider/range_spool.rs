// SPDX-License-Identifier: BUSL-1.1

//! Short-lived cache of VERIFIED delta reconstructions for range reads
//! (storage-11).
//!
//! A range GET of a large delta object reconstructs the whole object into a
//! spool file, checks its SHA-256, and serves one slice. A client that reads
//! the object in many ranges (a parallel downloader) paid one full decode per
//! range. This cache keeps the verified spool file for a short time, keyed by
//! `(bucket, key, content sha256)`, so the other ranges read the same file:
//!
//! - Only a spool that passed the integrity gate is inserted, so a range from
//!   the cache is as verified as a fresh decode.
//! - Single flight: a second range of the same key waits for the first
//!   decode instead of starting its own.
//! - The file keeps its spool-budget reservation while it is cached. The spool
//!   evicts idle entries first when another op finds the budget short
//!   ([`SpoolEvictor`]), and an entry expires after its TTL.
//! - An overwrite changes the content sha, so a new version never matches an
//!   old entry.

use crate::deltaglider::spool::{Spool, SpoolEvictor};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Most cached reconstructions at once.
pub(crate) const MAX_ENTRIES: usize = 16;

/// `(bucket, key, content sha256)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RangeSpoolKey {
    pub bucket: String,
    pub key: String,
    pub sha256: String,
}

struct Entry {
    spool: Arc<Spool>,
    created: Instant,
}

/// One key: the fill lock (single flight) and the cached spool.
#[derive(Default)]
struct Slot {
    entry: tokio::sync::Mutex<Option<Entry>>,
}

pub(crate) struct RangeSpoolCache {
    ttl: Duration,
    max_entries: usize,
    slots: parking_lot::Mutex<HashMap<RangeSpoolKey, Arc<Slot>>>,
    fills: AtomicU64,
    me: Weak<RangeSpoolCache>,
}

impl RangeSpoolCache {
    /// `ttl == 0` turns the cache off: every range decodes.
    pub(crate) fn new(ttl: Duration, max_entries: usize) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            ttl,
            max_entries,
            slots: Default::default(),
            fills: AtomicU64::new(0),
            me: me.clone(),
        })
    }

    /// How many times `fill` ran (decodes), for the tests.
    #[cfg(test)]
    pub(crate) fn fill_count(&self) -> u64 {
        self.fills.load(Ordering::SeqCst)
    }

    /// The verified spool of `key`: the cached one while it is fresh, else
    /// the result of `fill` (which must verify it), cached. A concurrent
    /// caller of the same key waits for the running fill. A failed fill is
    /// not cached; the next caller runs its own.
    pub(crate) async fn get_or_fill<F, Fut, E>(
        &self,
        key: RangeSpoolKey,
        fill: F,
    ) -> Result<Arc<Spool>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Spool, E>>,
    {
        if self.ttl.is_zero() {
            self.fills.fetch_add(1, Ordering::SeqCst);
            return fill().await.map(Arc::new);
        }
        let slot = self.slots.lock().entry(key.clone()).or_default().clone();
        let mut entry = slot.entry.lock().await;
        if let Some(e) = entry.as_ref() {
            if e.created.elapsed() < self.ttl {
                return Ok(e.spool.clone());
            }
        }
        // Expired (or empty): drop the old file before decoding a new one.
        let old = entry.take();
        drop(old);
        self.fills.fetch_add(1, Ordering::SeqCst);
        let spool = Arc::new(fill().await?);
        let created = Instant::now();
        *entry = Some(Entry {
            spool: spool.clone(),
            created,
        });
        drop(entry);
        self.schedule_expiry(key.clone(), created);
        // Over the bound: evict the oldest other entries.
        while self.len() > self.max_entries {
            let Some(victim) = self.take_oldest(Some(&key)) else {
                break;
            };
            drop(victim);
        }
        Ok(spool)
    }

    /// Cached entries (filled slots).
    fn len(&self) -> usize {
        self.slots
            .lock()
            .values()
            .filter(|s| s.entry.try_lock().map_or(true, |e| e.is_some()))
            .count()
    }

    /// Drop the entry of `key` at its TTL, unless it was refilled since.
    fn schedule_expiry(&self, key: RangeSpoolKey, created: Instant) {
        let me = self.me.clone();
        let ttl = self.ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            let Some(cache) = me.upgrade() else { return };
            let slot = cache.slots.lock().get(&key).cloned();
            let Some(slot) = slot else { return };
            let expired = {
                let mut entry = slot.entry.lock().await;
                match entry.as_ref() {
                    Some(e) if e.created == created => entry.take(),
                    _ => None,
                }
            };
            drop(expired);
            cache.prune_empty();
        });
    }

    /// Forget slots that hold nothing and that no caller uses.
    fn prune_empty(&self) {
        self.slots.lock().retain(|_, slot| {
            Arc::strong_count(slot) > 1 || slot.entry.try_lock().map_or(true, |e| e.is_some())
        });
    }

    /// Take the oldest idle entry (not `keep`; a slot that is filling is
    /// skipped). The caller drops it outside every lock: the drop deletes
    /// the file and releases its budget once no reader holds it.
    fn take_oldest(&self, keep: Option<&RangeSpoolKey>) -> Option<Arc<Spool>> {
        let slots: Vec<(RangeSpoolKey, Arc<Slot>)> = self
            .slots
            .lock()
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        let mut oldest: Option<(Instant, Arc<Slot>)> = None;
        for (k, slot) in slots {
            if keep == Some(&k) {
                continue;
            }
            let Ok(entry) = slot.entry.try_lock() else {
                continue;
            };
            if let Some(e) = entry.as_ref() {
                if oldest.as_ref().is_none_or(|(t, _)| e.created < *t) {
                    oldest = Some((e.created, slot.clone()));
                }
            }
        }
        let (_, slot) = oldest?;
        let taken = slot.entry.try_lock().ok()?.take().map(|e| e.spool);
        self.prune_empty();
        taken
    }
}

impl SpoolEvictor for RangeSpoolCache {
    fn evict_one(&self) -> bool {
        match self.take_oldest(None) {
            Some(spool) => {
                drop(spool);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deltaglider::spool::SpoolDir;

    fn key(k: &str) -> RangeSpoolKey {
        RangeSpoolKey {
            bucket: "b".into(),
            key: k.into(),
            sha256: "s".into(),
        }
    }

    const MIB: u64 = 1024 * 1024;

    #[tokio::test]
    async fn concurrent_callers_share_one_fill() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(60), MAX_ENTRIES);
        let fill = || async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sd.acquire(MIB).await
        };
        let got =
            futures::future::join_all((0..8).map(|_| cache.get_or_fill(key("a"), fill))).await;
        assert_eq!(cache.fill_count(), 1, "one decode for eight ranges");
        let first = got[0].as_ref().unwrap().path().to_path_buf();
        assert!(got.iter().all(|s| s.as_ref().unwrap().path() == first));
    }

    #[tokio::test]
    async fn a_failed_fill_is_not_cached_and_ttl_zero_never_caches() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(60), MAX_ENTRIES);
        let failed: Result<_, std::io::Error> = cache
            .get_or_fill(key("a"), || async { Err(std::io::Error::other("bad sha")) })
            .await;
        assert!(failed.is_err());
        cache
            .get_or_fill(key("a"), || sd.acquire(MIB))
            .await
            .unwrap();
        assert_eq!(cache.fill_count(), 2);

        let off = RangeSpoolCache::new(Duration::ZERO, MAX_ENTRIES);
        for _ in 0..3 {
            off.get_or_fill(key("a"), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(off.fill_count(), 3);
        assert_eq!(sd.free_mib(), 63, "only the cached entry holds budget");
    }

    #[tokio::test]
    async fn entries_are_bounded_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 64 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_millis(200), 2);
        for k in ["a", "b", "c"] {
            cache.get_or_fill(key(k), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(cache.len(), 2, "the oldest entry is evicted");
        assert_eq!(sd.free_mib(), 62);
        // The TTL drops the rest and releases their budget.
        let deadline = Instant::now() + Duration::from_secs(5);
        while sd.free_mib() < 64 {
            assert!(Instant::now() < deadline, "entries did not expire");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(cache.len(), 0);
    }

    /// A spool acquire that finds the budget short evicts idle cached
    /// reconstructions first, instead of waiting for their TTL.
    #[tokio::test]
    async fn a_short_budget_evicts_cached_spools_first() {
        let dir = tempfile::tempdir().unwrap();
        let sd = SpoolDir::new(dir.path().to_path_buf(), 4 * MIB).unwrap();
        let cache = RangeSpoolCache::new(Duration::from_secs(600), MAX_ENTRIES);
        sd.register_evictor(Arc::downgrade(&cache) as Weak<dyn SpoolEvictor>);
        for k in ["a", "b", "c"] {
            cache.get_or_fill(key(k), || sd.acquire(MIB)).await.unwrap();
        }
        assert_eq!(sd.free_mib(), 1);
        let big = tokio::time::timeout(Duration::from_secs(5), sd.acquire(3 * MIB))
            .await
            .expect("waited for budget that cached spools held");
        assert!(big.is_ok());
        assert_eq!(cache.len(), 1, "two entries went, the newest stays");
        // The no-wait path evicts too.
        drop(big);
        assert!(sd.try_acquire(4 * MIB).is_ok());
        assert_eq!(cache.len(), 0);
    }
}
