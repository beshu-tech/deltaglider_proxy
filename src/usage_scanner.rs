// SPDX-License-Identifier: GPL-3.0-only

//! Background usage scanner — computes prefix sizes asynchronously and caches results.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, warn};

use crate::api::handlers::AppState;
use crate::storage::StorageBackend as _;

/// Monotonic counter bumped after every completed usage-scan cache insert.
/// Sibling of `iam::IAM_VERSION` / `EXT_AUTH_VERSION`: lets integration tests
/// poll `GET /_/api/admin/usage-scan-version` for a deterministic scan-refresh
/// barrier instead of blind `sleep(500ms)` polls (CLAUDE.md testability rule:
/// "future async state changes … should follow the same pattern: a monotonic
/// counter bumped after the new state is published"). Process-local: the
/// scanner is a per-instance cache (per the HA contract).
static USAGE_SCAN_VERSION: AtomicU64 = AtomicU64::new(0);

/// Bump the scan-refresh counter. Called after a scan's result is inserted
/// into the cache (success OR truncated — both are a settled cache entry).
/// Returns the new version.
pub fn bump_usage_scan_version() -> u64 {
    USAGE_SCAN_VERSION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Current scan-refresh counter. Poll this to detect a completed scan.
pub fn current_usage_scan_version() -> u64 {
    USAGE_SCAN_VERSION.load(Ordering::SeqCst)
}

/// Cache TTL in seconds (5 minutes).
/// Cache TTL for usage-scan results. Default 5 minutes; quota tests
/// (and any operator who wants tighter enforcement) can shorten via
/// `DGP_USAGE_CACHE_TTL_SECS`. Lower values mean more frequent
/// re-scans on PUT/COPY but tighter quota enforcement after writes.
fn cache_ttl_secs() -> i64 {
    crate::config::env_parse_with_default("DGP_USAGE_CACHE_TTL_SECS", 300i64)
}

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

/// RAII guard that removes a (bucket, prefix) key from
/// `UsageScanner.scanning` on drop, including drop on panic unwind.
/// Pre-fix the cleanup was an explicit `.remove()` at the end of the
/// scan future — unreachable on panic, leaving the dedup key stuck
/// permanently (E-P1-2).
struct ScanInProgressGuard {
    scanner: Arc<UsageScanner>,
    key: String,
}

impl Drop for ScanInProgressGuard {
    fn drop(&mut self) {
        self.scanner.scanning.write().remove(&self.key);
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
            let stale_seconds = age - cache_ttl_secs();
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
        let stale_cutoff = Utc::now() - chrono::Duration::seconds(cache_ttl_secs() * 2);

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

            // E-P1-2: ensure the dedup key is removed from
            // `scanning` even if `do_scan` panics. Pre-fix the
            // cleanup at the bottom of this block was unreachable on
            // a panic unwind, so ANY panic anywhere in the scan
            // pipeline (storage backend, future poll, allocation
            // failure) left the (bucket, prefix) tuple permanently
            // marked as "in progress" until process restart. Future
            // calls returned None and never re-tried.
            //
            // The RAII guard runs `remove` on drop regardless of
            // whether the future completed normally, returned an
            // error, or unwound from a panic.
            let _scan_guard = ScanInProgressGuard {
                scanner: scanner.clone(),
                key: key.clone(),
            };

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
                    // Publish: a scan settled (success OR truncated) — bump the
                    // refresh counter so test barriers (and future operators)
                    // can detect it deterministically instead of blind-sleeping.
                    crate::usage_scanner::bump_usage_scan_version();
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
            // _scan_guard drops here, removing the dedup key. Same
            // semantics as the pre-fix explicit `remove` call but
            // panic-safe.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_scan_version_is_monotonic() {
        // The refresh counter must be strictly monotonic across bumps so a
        // `wait_for_usage_scan_refresh(baseline)` barrier can detect completion
        // by `current > baseline` without false positives.
        let a = bump_usage_scan_version();
        let b = bump_usage_scan_version();
        let c = bump_usage_scan_version();
        assert!(b > a, "bump must strictly increase: a={a} b={b}");
        assert!(c > b, "bump must strictly increase: b={b} c={c}");
        // `>=` not `==`: the counter is a process-global static, so a concurrent
        // test bumping it between our last bump and this read is benign — it can
        // only have advanced past `c`, never regressed.
        assert!(current_usage_scan_version() >= c);
    }

    fn make_entry(bucket: &str, prefix: &str, size: u64, objects: u64) -> UsageEntry {
        UsageEntry {
            prefix: prefix.to_string(),
            bucket: bucket.to_string(),
            total_size: size,
            total_objects: objects,
            children: HashMap::new(),
            computed_at: Utc::now(),
            stale_seconds: 0,
            truncated: false,
        }
    }

    #[test]
    fn test_get_returns_none_when_empty() {
        let scanner = UsageScanner::new();
        assert!(scanner.get("bucket", "").is_none());
    }

    #[test]
    fn test_get_returns_cached_entry() {
        let scanner = UsageScanner::new();
        let entry = make_entry("mybucket", "", 1024, 5);
        UsageScanner::insert_with_eviction(&scanner.cache, "mybucket/".to_string(), entry);

        let result = scanner.get("mybucket", "");
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.total_size, 1024);
        assert_eq!(r.total_objects, 5);
    }

    #[test]
    fn test_get_stale_seconds_positive_when_expired() {
        let scanner = UsageScanner::new();
        let mut entry = make_entry("mybucket", "", 100, 1);
        // Backdate to 10 minutes ago (TTL is 5 min = 300s)
        entry.computed_at = Utc::now() - chrono::Duration::seconds(600);
        UsageScanner::insert_with_eviction(&scanner.cache, "mybucket/".to_string(), entry);

        let result = scanner.get("mybucket", "").unwrap();
        // stale_seconds = age(600) - TTL(300) = 300
        assert!(
            result.stale_seconds >= 290,
            "stale_seconds should be ~300, got {}",
            result.stale_seconds
        );
    }

    #[test]
    fn test_cache_eviction_beyond_max_entries() {
        let scanner = UsageScanner::new();
        // Fill cache beyond MAX_CACHE_ENTRIES
        for i in 0..MAX_CACHE_ENTRIES + 5 {
            let entry = make_entry(&format!("bucket-{}", i), "", i as u64, 1);
            UsageScanner::insert_with_eviction(&scanner.cache, format!("bucket-{}/", i), entry);
        }
        let cache = scanner.cache.read();
        assert!(
            cache.len() <= MAX_CACHE_ENTRIES,
            "Cache should be at or below max: {} > {}",
            cache.len(),
            MAX_CACHE_ENTRIES
        );
    }

    #[test]
    fn test_is_scanning_dedup() {
        let scanner = UsageScanner::new();
        // Mark as scanning
        scanner
            .scanning
            .write()
            .insert("mybucket/prefix/".to_string());
        assert!(scanner.is_scanning("mybucket", "prefix/"));
        assert!(!scanner.is_scanning("mybucket", "other/"));
    }

    #[test]
    fn test_insert_cleans_stale_entries() {
        let scanner = UsageScanner::new();
        // Insert an entry backdated beyond 2x TTL (should be cleaned)
        let mut stale = make_entry("stale", "", 100, 1);
        stale.computed_at = Utc::now() - chrono::Duration::seconds(cache_ttl_secs() * 3);
        UsageScanner::insert_with_eviction(&scanner.cache, "stale/".to_string(), stale);

        // Insert a fresh entry — the stale one should be cleaned
        let fresh = make_entry("fresh", "", 200, 2);
        UsageScanner::insert_with_eviction(&scanner.cache, "fresh/".to_string(), fresh);

        let cache = scanner.cache.read();
        assert!(
            cache.get("stale/").is_none(),
            "Stale entry should be cleaned"
        );
        assert!(cache.get("fresh/").is_some(), "Fresh entry should exist");
    }

    /// E-P1-2 regression: even when the scan future panics, the
    /// dedup key must be removed from `scanning`. Pre-fix the
    /// cleanup line at the bottom of the spawned future was
    /// unreachable on panic; the (bucket, prefix) tuple stayed
    /// permanently marked as "in progress" and ALL subsequent
    /// scans of that prefix returned `false` from `enqueue_scan`
    /// until process restart.
    ///
    /// The fix is the `ScanInProgressGuard` Drop impl. Test it by
    /// constructing the guard, simulating a panic via
    /// `std::panic::catch_unwind`, and verifying the key is gone
    /// after the unwind.
    #[test]
    fn scan_in_progress_guard_clears_dedup_key_on_panic() {
        let scanner = Arc::new(UsageScanner::new());
        let key = "bucket/prefix/".to_string();

        // Seed the scanning set as enqueue_scan would have.
        scanner.scanning.write().insert(key.clone());
        assert!(scanner.scanning.read().contains(&key));

        // Now simulate the panic-unwind path. The guard owns the
        // arc + key; when the closure panics, Rust unwinds and
        // drops the guard, which calls `remove`.
        let scanner_for_panic = Arc::clone(&scanner);
        let key_for_panic = key.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = ScanInProgressGuard {
                scanner: scanner_for_panic,
                key: key_for_panic,
            };
            panic!("simulated do_scan panic — Drop must still run");
        }));
        assert!(result.is_err(), "panic must propagate (caught here)");

        // Post-condition: the dedup key is gone, so a future
        // enqueue_scan of the same (bucket, prefix) would proceed.
        assert!(
            !scanner.scanning.read().contains(&key),
            "ScanInProgressGuard must clear dedup key on panic unwind"
        );
    }
}
