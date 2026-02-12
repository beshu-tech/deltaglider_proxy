//! LRU cache for reference files

use bytes::Bytes;
use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use tracing::debug;

/// Internal cache state protected by a single mutex to prevent deadlocks
struct CacheInner {
    cache: LruCache<String, Bytes>,
    current_size: usize,
}

/// LRU cache for frequently accessed reference files.
/// Uses `Bytes` for zero-copy cloning (refcount increment instead of memcpy).
pub struct ReferenceCache {
    inner: Mutex<CacheInner>,
    max_size_bytes: usize,
}

impl ReferenceCache {
    /// Create a new cache with the given maximum size in megabytes
    pub fn new(max_size_mb: usize) -> Self {
        // Estimate max entries based on average reference size (assume ~1MB avg)
        let max_entries = max_size_mb.max(1);
        Self {
            inner: Mutex::new(CacheInner {
                cache: LruCache::new(NonZeroUsize::new(max_entries).unwrap()),
                current_size: 0,
            }),
            max_size_bytes: max_size_mb * 1024 * 1024,
        }
    }

    /// Get a reference from cache. Returns a `Bytes` handle (cheap refcount clone).
    pub fn get(&self, prefix: &str) -> Option<Bytes> {
        let mut inner = self.inner.lock();
        let result = inner.cache.get(prefix).cloned();
        if result.is_some() {
            debug!("Cache hit for prefix: {}", prefix);
        } else {
            debug!("Cache miss for prefix: {}", prefix);
        }
        result
    }

    /// Put a reference into cache
    pub fn put(&self, prefix: &str, data: Vec<u8>) {
        let data_size = data.len();

        // Don't cache if single item exceeds max size
        if data_size > self.max_size_bytes {
            debug!(
                "Reference too large for cache: {} bytes (max: {} bytes)",
                data_size, self.max_size_bytes
            );
            return;
        }

        let mut inner = self.inner.lock();

        // Evict entries until we have space
        while inner.current_size + data_size > self.max_size_bytes {
            if let Some((evicted_key, evicted_data)) = inner.cache.pop_lru() {
                inner.current_size = inner.current_size.saturating_sub(evicted_data.len());
                debug!(
                    "Evicted {} from cache ({} bytes)",
                    evicted_key,
                    evicted_data.len()
                );
            } else {
                break;
            }
        }

        // Add to cache (convert Vec<u8> → Bytes for zero-copy sharing)
        let bytes_data = Bytes::from(data);
        if let Some(old) = inner.cache.put(prefix.to_string(), bytes_data) {
            inner.current_size = inner.current_size.saturating_sub(old.len());
        }
        inner.current_size += data_size;

        debug!(
            "Cached reference for {}: {} bytes (total cache: {} bytes)",
            prefix, data_size, inner.current_size
        );
    }

    /// Invalidate a cache entry
    pub fn invalidate(&self, prefix: &str) {
        let mut inner = self.inner.lock();

        if let Some(data) = inner.cache.pop(prefix) {
            inner.current_size = inner.current_size.saturating_sub(data.len());
            debug!("Invalidated cache entry for {}", prefix);
        }
    }

}
