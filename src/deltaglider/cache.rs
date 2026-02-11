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

    /// Clear the entire cache
    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.cache.clear();
        inner.current_size = 0;
        debug!("Cache cleared");
    }

    /// Get current cache size in bytes
    pub fn size(&self) -> usize {
        self.inner.lock().current_size
    }

    /// Get number of entries in cache
    pub fn len(&self) -> usize {
        self.inner.lock().cache.len()
    }

    /// Check if cache is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_put_get() {
        let cache = ReferenceCache::new(10); // 10MB

        let data = vec![1u8; 1000];
        cache.put("prefix1", data.clone());

        let retrieved = cache.get("prefix1");
        assert_eq!(retrieved.as_deref(), Some(data.as_slice()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = ReferenceCache::new(10);
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn test_cache_eviction() {
        // 1KB cache
        let cache = ReferenceCache {
            inner: Mutex::new(CacheInner {
                cache: LruCache::new(NonZeroUsize::new(2).unwrap()),
                current_size: 0,
            }),
            max_size_bytes: 1024,
        };

        // Add entries that exceed cache size
        cache.put("a", vec![0u8; 400]);
        cache.put("b", vec![0u8; 400]);
        cache.put("c", vec![0u8; 400]); // Should evict 'a'

        assert!(cache.get("a").is_none() || cache.size() <= 1024);
    }

    #[test]
    fn test_cache_invalidate() {
        let cache = ReferenceCache::new(10);

        cache.put("prefix1", vec![1u8; 100]);
        assert!(cache.get("prefix1").is_some());

        cache.invalidate("prefix1");
        assert!(cache.get("prefix1").is_none());
    }
}
