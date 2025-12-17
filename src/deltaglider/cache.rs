//! LRU cache for reference files

use lru::LruCache;
use parking_lot::Mutex;
use std::num::NonZeroUsize;
use tracing::debug;

/// LRU cache for frequently accessed reference files
pub struct ReferenceCache {
    cache: Mutex<LruCache<String, Vec<u8>>>,
    max_size_bytes: usize,
    current_size: Mutex<usize>,
}

impl ReferenceCache {
    /// Create a new cache with the given maximum size in megabytes
    pub fn new(max_size_mb: usize) -> Self {
        // Estimate max entries based on average reference size (assume ~1MB avg)
        let max_entries = max_size_mb.max(1);
        Self {
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(max_entries).unwrap())),
            max_size_bytes: max_size_mb * 1024 * 1024,
            current_size: Mutex::new(0),
        }
    }

    /// Get a reference from cache
    pub fn get(&self, prefix: &str) -> Option<Vec<u8>> {
        let mut cache = self.cache.lock();
        let result = cache.get(prefix).cloned();
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

        let mut cache = self.cache.lock();
        let mut current_size = self.current_size.lock();

        // Evict entries until we have space
        while *current_size + data_size > self.max_size_bytes {
            if let Some((evicted_key, evicted_data)) = cache.pop_lru() {
                *current_size = current_size.saturating_sub(evicted_data.len());
                debug!(
                    "Evicted {} from cache ({} bytes)",
                    evicted_key,
                    evicted_data.len()
                );
            } else {
                break;
            }
        }

        // Add to cache
        if let Some(old) = cache.put(prefix.to_string(), data) {
            *current_size = current_size.saturating_sub(old.len());
        }
        *current_size += data_size;

        debug!(
            "Cached reference for {}: {} bytes (total cache: {} bytes)",
            prefix, data_size, *current_size
        );
    }

    /// Invalidate a cache entry
    pub fn invalidate(&self, prefix: &str) {
        let mut cache = self.cache.lock();
        let mut current_size = self.current_size.lock();

        if let Some(data) = cache.pop(prefix) {
            *current_size = current_size.saturating_sub(data.len());
            debug!("Invalidated cache entry for {}", prefix);
        }
    }

    /// Clear the entire cache
    pub fn clear(&self) {
        let mut cache = self.cache.lock();
        let mut current_size = self.current_size.lock();
        cache.clear();
        *current_size = 0;
        debug!("Cache cleared");
    }

    /// Get current cache size in bytes
    pub fn size(&self) -> usize {
        *self.current_size.lock()
    }

    /// Get number of entries in cache
    pub fn len(&self) -> usize {
        self.cache.lock().len()
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
        assert_eq!(retrieved, Some(data));
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
            cache: Mutex::new(LruCache::new(NonZeroUsize::new(2).unwrap())),
            max_size_bytes: 1024,
            current_size: Mutex::new(0),
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
