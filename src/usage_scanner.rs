//! Background usage scanner — computes prefix sizes asynchronously and caches results.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, warn};

use crate::api::handlers::AppState;
use crate::storage::StorageBackend as _;

/// Cache TTL in seconds (5 minutes).
const CACHE_TTL_SECS: i64 = 300;

/// Maximum number of entries in the usage cache. When exceeded, the oldest
/// entry (by `computed_at`) is evicted before inserting a new one.
const MAX_CACHE_ENTRIES: usize = 1000;

/// Maximum number of objects to process in a single scan. If the prefix
/// contains more objects than this, the result is truncated and marked
/// accordingly to prevent OOM on large prefixes.
const MAX_SCAN_OBJECTS: usize = 100_000;

/// Result of a prefix usage scan — sizes grouped by immediate child prefix.
#[derive(Clone, Serialize)]
pub struct UsageEntry {
    pub prefix: String,
    pub bucket: String,
    pub total_size: u64,
    pub total_objects: u64,
    pub children: HashMap<String, ChildUsage>,
    pub computed_at: DateTime<Utc>,
    /// How many seconds ago this entry was computed. Positive = stale by this many seconds
    /// beyond the TTL. Populated on read, not on write.
    pub stale_seconds: i64,
    /// True if the scan was truncated because the prefix contained more than
    /// `MAX_SCAN_OBJECTS` objects. The totals represent a lower bound.
    #[serde(default)]
    pub truncated: bool,
}

/// Size and object count for an immediate child prefix.
#[derive(Clone, Serialize)]
pub struct ChildUsage {
    pub size: u64,
    pub objects: u64,
}

/// Background usage scanner with in-memory cache and scan deduplication.
pub struct UsageScanner {
    cache: Arc<RwLock<HashMap<String, UsageEntry>>>,
    scanning: Arc<RwLock<HashSet<String>>>,
}

impl Default for UsageScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl UsageScanner {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            scanning: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Build the cache key for a bucket/prefix pair.
    fn cache_key(bucket: &str, prefix: &str) -> String {
        format!("{}/{}", bucket, prefix)
    }

    /// Get a cached entry if it exists.
    /// Returns `None` if not cached.
    /// The `stale_seconds` field indicates how stale the entry is beyond the TTL.
    pub fn get(&self, bucket: &str, prefix: &str) -> Option<UsageEntry> {
        let key = Self::cache_key(bucket, prefix);
        let cache = self.cache.read();
        if let Some(entry) = cache.get(&key) {
            let age = Utc::now()
                .signed_duration_since(entry.computed_at)
                .num_seconds();
            let stale_seconds = age - CACHE_TTL_SECS;
            let mut result = entry.clone();
            result.stale_seconds = stale_seconds;
            Some(result)
        } else {
            None
        }
    }

    /// Returns true if a scan for this bucket/prefix is already in progress.
    pub fn is_scanning(&self, bucket: &str, prefix: &str) -> bool {
        let key = Self::cache_key(bucket, prefix);
        self.scanning.read().contains(&key)
    }

    /// Insert an entry into the cache, evicting the oldest entry if the cache
    /// exceeds `MAX_CACHE_ENTRIES`. Also removes entries older than 2x TTL
    /// to prevent stale data from lingering.
    fn insert_with_eviction(
        cache: &RwLock<HashMap<String, UsageEntry>>,
        key: String,
        entry: UsageEntry,
    ) {
        let mut cache = cache.write();
        let stale_cutoff = Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS * 2);

        // Periodic cleanup: remove entries older than 2x TTL (10 minutes)
        cache.retain(|_, v| v.computed_at > stale_cutoff);

        // If still over capacity, evict the oldest entry by computed_at
        if cache.len() >= MAX_CACHE_ENTRIES {
            if let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, v)| v.computed_at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&oldest_key);
            }
        }

        cache.insert(key, entry);
    }

    /// Get cached usage for a bucket/prefix. If not cached, triggers a background
    /// scan and returns `None` (the scan result will be available on next call).
    /// Used by quota checks — returns stale data rather than blocking on a scan.
    pub fn get_or_scan(
        self: &Arc<Self>,
        s3_state: &Arc<AppState>,
        bucket: &str,
        prefix: &str,
    ) -> Option<UsageEntry> {
        let cached = self.get(bucket, prefix);
        if cached.is_none() || cached.as_ref().is_some_and(|e| e.stale_seconds > 0) {
            // Trigger background scan when no cache or cache is stale
            self.enqueue_scan(bucket.to_string(), prefix.to_string(), s3_state.clone());
        }
        cached
    }

    /// Enqueue a background scan for the given bucket/prefix.
    /// Returns `true` if a new scan was started, `false` if one is already running.
    pub fn enqueue_scan(
        self: &Arc<Self>,
        bucket: String,
        prefix: String,
        s3_state: Arc<AppState>,
    ) -> bool {
        let key = Self::cache_key(&bucket, &prefix);

        // Dedup: skip if already scanning this prefix
        {
            let mut scanning = self.scanning.write();
            if !scanning.insert(key.clone()) {
                debug!(
                    bucket = %bucket,
                    prefix = %prefix,
                    "Usage scan already in progress, skipping duplicate"
                );
                return false;
            }
        }

        let scanner = Arc::clone(self);
        tokio::spawn(async move {
            debug!(bucket = %bucket, prefix = %prefix, "Starting usage scan");
            let result = Self::do_scan(&s3_state, &bucket, &prefix).await;
            match result {
                Ok(entry) => {
                    debug!(
                        bucket = %bucket,
                        prefix = %prefix,
                        total_size = entry.total_size,
                        total_objects = entry.total_objects,
                        children = entry.children.len(),
                        truncated = entry.truncated,
                        "Usage scan complete"
                    );
                    Self::insert_with_eviction(&scanner.cache, key.clone(), entry);
                }
                Err(e) => {
                    warn!(
                        bucket = %bucket,
                        prefix = %prefix,
                        error = %e,
                        "Usage scan failed"
                    );
                }
            }
            scanner.scanning.write().remove(&key);
        });

        true
    }

    /// Perform the actual scan: list all objects under the prefix and group by
    /// immediate child prefix. Limits processing to `MAX_SCAN_OBJECTS` to
    /// prevent OOM on very large prefixes.
    async fn do_scan(
        s3_state: &AppState,
        bucket: &str,
        prefix: &str,
    ) -> Result<UsageEntry, String> {
        let engine = s3_state.engine.load();
        let objects = engine
            .storage()
            .bulk_list_objects(bucket, prefix)
            .await
            .map_err(|e| format!("bulk_list_objects failed: {e}"))?;

        let truncated = objects.len() > MAX_SCAN_OBJECTS;
        if truncated {
            warn!(
                bucket = %bucket,
                prefix = %prefix,
                total = objects.len(),
                limit = MAX_SCAN_OBJECTS,
                "Scan truncated: prefix contains more objects than MAX_SCAN_OBJECTS"
            );
        }

        let mut total_size: u64 = 0;
        let mut total_objects: u64 = 0;
        let mut children: HashMap<String, ChildUsage> = HashMap::new();

        for (key, meta) in objects.iter().take(MAX_SCAN_OBJECTS) {
            let obj_size = meta.file_size;
            total_size += obj_size;
            total_objects += 1;

            // Determine the immediate child prefix.
            // If prefix is "builds/" and key is "builds/v1/foo.zip",
            // the child is "builds/v1/".
            let relative = if key.len() > prefix.len() {
                &key[prefix.len()..]
            } else {
                continue;
            };

            if let Some(slash_pos) = relative.find('/') {
                let child_prefix = format!("{}{}/", prefix, &relative[..slash_pos]);
                let child = children.entry(child_prefix).or_insert(ChildUsage {
                    size: 0,
                    objects: 0,
                });
                child.size += obj_size;
                child.objects += 1;
            }
            // Objects directly under the prefix (no slash in relative) are counted
            // in total but don't form a child prefix — they're leaf objects.
        }

        Ok(UsageEntry {
            prefix: prefix.to_string(),
            bucket: bucket.to_string(),
            total_size,
            total_objects,
            children,
            computed_at: Utc::now(),
            stale_seconds: 0,
            truncated,
        })
    }
}
