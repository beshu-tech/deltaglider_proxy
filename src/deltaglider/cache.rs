//! LRU cache for reference files (backed by moka)
//!
//! PERF: This replaced a hand-rolled `parking_lot::Mutex<LruCache>` with
//! `moka::sync::Cache`. Two critical improvements:
//!
//! 1. **Lock-free reads**: moka uses concurrent hash maps internally, so GET
//!    operations never block each other. The old Mutex serialized ALL cache
//!    access (reads AND writes) behind one global lock — a bottleneck under
//!    concurrent GET requests that all need reference data.
//!
//! 2. **Correct byte-budget eviction**: The old LRU used `max_entries = max_size_mb`
//!    which treated 1 entry = 1 MB regardless of actual size. A 50KB entry and a
//!    50MB entry both counted as "1 unit". moka's weigher tracks actual byte size.
//!
//! Do NOT replace moka with a simple Mutex<HashMap> or Mutex<LruCache> — it will
//! re-introduce the global-lock serialization bottleneck on every GET request.

use bytes::Bytes;
use moka::sync::Cache;
use tracing::debug;

/// Concurrent LRU cache for frequently accessed reference files.
///
/// Uses `moka::sync::Cache` with byte-budget eviction (weigher) for
/// lock-free concurrent reads and automatic size-bounded eviction.
///
/// PERF: Values are `Bytes` (not `Vec<u8>`) because `Bytes::clone()` is a
/// cheap refcount increment (~1 ns) instead of a full memcpy. This matters
/// because cached references are read on every delta GET request. Callers
/// that already have a `Vec<u8>` can use `Bytes::from(vec)` for zero-copy
/// conversion (ownership transfer, no copy). Callers with `&[u8]` must use
/// `Bytes::copy_from_slice()` (one unavoidable copy).
/// Do NOT change put() to accept `Vec<u8>` — it defeats the zero-copy path.
pub struct ReferenceCache {
    cache: Cache<String, Bytes>,
}

impl ReferenceCache {
    /// Create a new cache with the given maximum size in megabytes.
    pub fn new(max_size_mb: usize) -> Self {
        let max_size_bytes = (max_size_mb as u64) * 1024 * 1024;

        let cache = Cache::builder()
            // moka uses max_capacity as the total weight budget (in bytes here).
            .max_capacity(max_size_bytes)
            .weigher(|_key: &String, value: &Bytes| -> u32 {
                // Each entry's weight = its actual byte length.
                // moka weigher returns u32; clamp to u32::MAX for entries that
                // (in theory) exceed 4 GiB. This is a moka API constraint, not
                // a bug — entries >4GiB would be rejected by the engine's
                // max_object_size limit long before reaching the cache.
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .build();

        Self { cache }
    }

    /// Get a reference from cache. Returns a `Bytes` handle (cheap refcount clone).
    pub fn get(&self, prefix: &str) -> Option<Bytes> {
        let result = self.cache.get(&prefix.to_string());
        if result.is_some() {
            debug!("Cache hit for prefix: {}", prefix);
        } else {
            debug!("Cache miss for prefix: {}", prefix);
        }
        result
    }

    /// Put a reference into cache.
    ///
    /// PERF: Takes `Bytes` (not `Vec<u8>`) to enable zero-copy insertion when the
    /// caller already owns a Vec (via `Bytes::from(vec)` — ownership transfer, no
    /// memcpy). See struct-level doc comment for details.
    pub fn put(&self, prefix: &str, data: Bytes) {
        let data_len = data.len();
        debug!("Cached reference for {}: {} bytes", prefix, data_len);
        self.cache.insert(prefix.to_string(), data);
    }

    /// Return the number of entries in the cache (O(1) atomic read).
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Return the total weighted size of cached entries in bytes (O(1) atomic read).
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Invalidate a cache entry.
    pub fn invalidate(&self, prefix: &str) {
        self.cache.invalidate(&prefix.to_string());
        debug!("Invalidated cache entry for {}", prefix);
    }

    /// Synchronously run all pending eviction/maintenance tasks.
    /// Only exposed for tests — production code relies on moka's lazy maintenance.
    #[cfg(test)]
    fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byte_budget_eviction() {
        let cache = ReferenceCache::new(1);
        for i in 0..100u8 {
            cache.put(&format!("key_{}", i), Bytes::from(vec![i; 20 * 1024]));
        }
        cache.run_pending_tasks();
        let count = (0..100u8)
            .filter(|i| cache.get(&format!("key_{}", i)).is_some())
            .count();
        assert!(count < 100, "eviction should have removed some entries");
        assert!(count > 0, "cache should still contain some entries");
    }

    #[test]
    fn test_large_entry_eviction() {
        let cache = ReferenceCache::new(1); // 1 MB budget
        cache.put("big", Bytes::from(vec![0xAA; 500 * 1024])); // 500 KB
                                                               // Fill the remaining ~500 KB, then keep inserting to force eviction.
                                                               // Inserting 2 MB total of 100 KB entries (20 entries) ensures the
                                                               // cache must evict aggressively — "big" is the highest-value eviction
                                                               // target since it frees the most space in one shot.
        for i in 0..20 {
            cache.put(&format!("fill_{}", i), Bytes::from(vec![0xBB; 100 * 1024]));
        }
        // Drain moka's async eviction queue
        for _ in 0..20 {
            cache.run_pending_tasks();
        }
        // Count surviving entries — the total cached bytes must respect the 1 MB budget.
        // We can't predict exact eviction order (moka uses TinyLFU, not strict LRU),
        // but the total weight must be bounded.
        let mut total_bytes: usize = 0;
        if cache.get("big").is_some() {
            total_bytes += 500 * 1024;
        }
        for i in 0..20 {
            if cache.get(&format!("fill_{}", i)).is_some() {
                total_bytes += 100 * 1024;
            }
        }
        // moka may temporarily overshoot the budget by one entry's weight,
        // but the total should be roughly within 1 MB + one entry slack.
        assert!(
            total_bytes <= 1_200 * 1024,
            "total cached bytes {} should be near the 1 MB budget",
            total_bytes
        );
        // Not everything should have been evicted
        assert!(total_bytes > 0, "cache should still contain some entries");
    }

    #[test]
    fn test_concurrent_cache_operations() {
        let cache = ReferenceCache::new(10);
        std::thread::scope(|s| {
            for t in 0..16u8 {
                let cache = &cache;
                s.spawn(move || {
                    for j in 0..1000usize {
                        let key = format!("key_{}", j % 50);
                        match j % 3 {
                            0 => cache.put(
                                &key,
                                Bytes::from(vec![t.wrapping_mul(17).wrapping_add(j as u8); 100]),
                            ),
                            1 => {
                                cache.get(&key);
                            }
                            _ => cache.invalidate(&key),
                        }
                    }
                });
            }
        });
        // If we got here without panic, concurrent operations are safe
    }

    #[test]
    fn test_bytes_from_vec_and_copy_from_slice_equivalent() {
        let cache = ReferenceCache::new(10);
        let data = vec![1u8, 2, 3, 4, 5];
        cache.put("vec_path", Bytes::from(data.clone()));
        let from_vec = cache.get("vec_path").unwrap();
        cache.put("slice_path", Bytes::copy_from_slice(&data));
        let from_slice = cache.get("slice_path").unwrap();
        assert_eq!(from_vec, from_slice);
        assert_eq!(&from_vec[..], &data[..]);
    }

    #[test]
    fn test_invalidation_is_immediate() {
        let cache = ReferenceCache::new(10);
        cache.put("key", Bytes::from_static(b"hello"));
        assert!(cache.get("key").is_some());
        cache.invalidate("key");
        assert!(
            cache.get("key").is_none(),
            "invalidation should be immediate"
        );
    }

    #[test]
    fn test_zero_byte_entry() {
        let cache = ReferenceCache::new(10);
        cache.put("empty", Bytes::new());
        let result = cache.get("empty");
        assert!(result.is_some(), "empty entry should be retrievable");
        assert!(
            result.unwrap().is_empty(),
            "empty entry should have zero length"
        );
    }

    #[test]
    fn test_weigher_clamp_no_panic() {
        let cache = ReferenceCache::new(1000);
        let sizes = [0, 1, 100, 10_000, 100_000];
        for (i, &size) in sizes.iter().enumerate() {
            let key = format!("entry_{}", i);
            cache.put(&key, Bytes::from(vec![0xCC; size]));
        }
        for (i, &size) in sizes.iter().enumerate() {
            let key = format!("entry_{}", i);
            let result = cache.get(&key);
            assert!(
                result.is_some(),
                "entry with size {} should be retrievable",
                size
            );
            assert_eq!(result.unwrap().len(), size);
        }
    }
}
