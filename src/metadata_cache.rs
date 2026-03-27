//! In-memory metadata cache backed by moka
//!
//! Eliminates HEAD requests for metadata enrichment across the system.
//! Populated on store/head/enrich, invalidated on delete/overwrite.
//!
//! Budget: ~50 MB, ~125K–150K entries (FileMetadata is ~300–500 bytes each).
//! TTL: 10 minutes — stale metadata is harmless (worst case: one extra HEAD).

use crate::types::FileMetadata;
use moka::sync::Cache;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

/// Concurrent in-memory cache for object metadata (FileMetadata).
///
/// Key format: `"bucket/key"` — no collision risk because S3 bucket names
/// cannot contain `/`.
///
/// Thread-safe and lock-free for reads (same moka backend as ReferenceCache).
#[derive(Clone)]
pub struct MetadataCache {
    cache: Cache<String, FileMetadata>,
}

impl MetadataCache {
    /// Create a new metadata cache with the given byte budget.
    ///
    /// Uses a conservative per-entry weight of 400 bytes for eviction accounting.
    /// With a 50 MB budget this yields ~125K entries before eviction kicks in.
    pub fn new(max_bytes: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|key: &String, _value: &FileMetadata| -> u32 {
                // Approximate weight: key length + fixed struct overhead.
                // FileMetadata contains several Strings and a HashMap, so the
                // actual heap size varies. We use key.len() + 350 as a rough
                // estimate to avoid both over- and under-counting.
                let key_bytes = key.len() as u32;
                key_bytes.saturating_add(350)
            })
            .time_to_live(Duration::from_secs(600)) // 10 min TTL
            .build();
        Self { cache }
    }

    /// Build the cache key from bucket and object key.
    fn cache_key(bucket: &str, key: &str) -> String {
        format!("{}/{}", bucket, key)
    }

    /// Look up cached metadata for an object.
    pub fn get(&self, bucket: &str, key: &str) -> Option<FileMetadata> {
        let ck = Self::cache_key(bucket, key);
        let result = self.cache.get(&ck);
        if result.is_some() {
            debug!("Metadata cache hit: {}", ck);
        }
        result
    }

    /// Insert or update metadata for an object.
    pub fn insert(&self, bucket: &str, key: &str, metadata: FileMetadata) {
        let ck = Self::cache_key(bucket, key);
        debug!("Metadata cache insert: {}", ck);
        self.cache.insert(ck, metadata);
    }

    /// Invalidate (remove) cached metadata for a single object.
    pub fn invalidate(&self, bucket: &str, key: &str) {
        let ck = Self::cache_key(bucket, key);
        self.cache.invalidate(&ck);
        debug!("Metadata cache invalidate: {}", ck);
    }

    /// Invalidate all cached entries whose key starts with `bucket/prefix`.
    ///
    /// Used for folder/prefix deletes. Iterates all entries (moka has no
    /// native prefix query), but this is acceptable because prefix deletes
    /// are infrequent bulk operations.
    pub fn invalidate_prefix(&self, bucket: &str, prefix: &str) {
        let prefix_str = Self::cache_key(bucket, prefix);
        let mut count = 0u64;
        // Use iter() + invalidate() — avoids requiring support_invalidation_closures()
        // on the cache builder, which adds memory overhead to every entry.
        for (key, _) in &self.cache {
            if key.starts_with(&prefix_str) {
                self.cache.invalidate(key.as_str());
                count += 1;
            }
        }
        if count > 0 {
            debug!(
                "Metadata cache prefix invalidate: {} ({} entries)",
                prefix_str, count
            );
        }
    }

    /// Return the number of entries currently in the cache.
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Return the total weighted size in bytes.
    pub fn weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Synchronously run pending eviction tasks (test helper).
    #[cfg(test)]
    fn run_pending_tasks(&self) {
        self.cache.run_pending_tasks();
    }
}

/// Shared metadata cache handle (cheaply cloneable via Arc).
pub type SharedMetadataCache = Arc<MetadataCache>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileMetadata, StorageInfo};

    fn sample_metadata(name: &str) -> FileMetadata {
        FileMetadata::new_passthrough(
            name.to_string(),
            "abc123".to_string(),
            "def456".to_string(),
            1024,
            Some("application/octet-stream".to_string()),
        )
    }

    #[test]
    fn test_cache_insert_get() {
        let cache = MetadataCache::new(10 * 1024 * 1024); // 10 MB
        let meta = sample_metadata("file.zip");

        assert!(cache.get("mybucket", "prefix/file.zip").is_none());

        cache.insert("mybucket", "prefix/file.zip", meta.clone());

        let cached = cache.get("mybucket", "prefix/file.zip");
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.original_name, "file.zip");
        assert_eq!(cached.file_size, 1024);
        assert!(matches!(cached.storage_info, StorageInfo::Passthrough));
    }

    #[test]
    fn test_cache_invalidate() {
        let cache = MetadataCache::new(10 * 1024 * 1024);
        let meta = sample_metadata("file.zip");

        cache.insert("mybucket", "prefix/file.zip", meta);
        assert!(cache.get("mybucket", "prefix/file.zip").is_some());

        cache.invalidate("mybucket", "prefix/file.zip");
        assert!(cache.get("mybucket", "prefix/file.zip").is_none());
    }

    #[test]
    fn test_cache_invalidate_prefix() {
        let cache = MetadataCache::new(10 * 1024 * 1024);

        // Insert entries under two different prefixes
        cache.insert("mybucket", "releases/v1/a.zip", sample_metadata("a.zip"));
        cache.insert("mybucket", "releases/v1/b.zip", sample_metadata("b.zip"));
        cache.insert("mybucket", "releases/v2/c.zip", sample_metadata("c.zip"));
        cache.insert("mybucket", "other/d.zip", sample_metadata("d.zip"));

        // Invalidate all entries under releases/v1/
        cache.invalidate_prefix("mybucket", "releases/v1/");

        assert!(cache.get("mybucket", "releases/v1/a.zip").is_none());
        assert!(cache.get("mybucket", "releases/v1/b.zip").is_none());
        // These should still exist
        assert!(cache.get("mybucket", "releases/v2/c.zip").is_some());
        assert!(cache.get("mybucket", "other/d.zip").is_some());
    }

    #[test]
    fn test_cache_capacity_eviction() {
        // Tiny cache: 4 KB budget. Each entry weighs ~350 + key_len bytes,
        // so only ~10 entries fit.
        let cache = MetadataCache::new(4 * 1024);

        for i in 0..100u32 {
            let key = format!("prefix/file_{}.zip", i);
            cache.insert("bucket", &key, sample_metadata(&format!("file_{}.zip", i)));
        }
        cache.run_pending_tasks();

        // Not all 100 entries should survive in a 4 KB cache
        let surviving = (0..100u32)
            .filter(|i| {
                cache
                    .get("bucket", &format!("prefix/file_{}.zip", i))
                    .is_some()
            })
            .count();
        assert!(
            surviving < 100,
            "eviction should have removed some entries, but {} survived",
            surviving
        );
        assert!(
            surviving > 0,
            "cache should still contain some entries after eviction"
        );
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = MetadataCache::new(10 * 1024 * 1024);

        let meta1 = sample_metadata("file.zip");
        cache.insert("bucket", "key", meta1);

        let mut meta2 = sample_metadata("file.zip");
        meta2.file_size = 2048;
        cache.insert("bucket", "key", meta2);

        let cached = cache.get("bucket", "key").unwrap();
        assert_eq!(cached.file_size, 2048, "overwrite should update the entry");
    }

    #[test]
    fn test_cache_different_buckets() {
        let cache = MetadataCache::new(10 * 1024 * 1024);
        let meta = sample_metadata("file.zip");

        cache.insert("bucket-a", "key", meta.clone());
        cache.insert("bucket-b", "key", sample_metadata("other.zip"));

        let a = cache.get("bucket-a", "key").unwrap();
        assert_eq!(a.original_name, "file.zip");
        let b = cache.get("bucket-b", "key").unwrap();
        assert_eq!(b.original_name, "other.zip");
    }
}
