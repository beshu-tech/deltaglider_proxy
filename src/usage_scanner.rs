// SPDX-License-Identifier: BUSL-1.1

//! Background usage scanner — computes prefix sizes asynchronously and caches results.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tracing::{debug, warn};

use crate::api::handlers::AppState;
use crate::storage::list_size_cache::ListedSize;
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
///
/// Sizes follow the rule the rest of the UI uses for files: `total_size` and
/// `ChildUsage::size` are LOGICAL bytes — the original, pre-delta size a
/// client downloads. `stored_size` is the separate on-backend footprint
/// (deltas, passthrough objects and the `reference.bin` baselines).
#[derive(Clone, Serialize)]
pub struct UsageEntry {
    pub prefix: String,
    pub bucket: String,
    /// Logical (original) bytes of every object under the prefix.
    pub total_size: u64,
    /// Bytes stored on the backend, delta baselines included.
    pub stored_size: u64,
    pub total_objects: u64,
    pub children: HashMap<String, ChildUsage>,
    pub computed_at: DateTime<Utc>,
    /// Seconds since the entry was computed. Populated on read.
    pub age_seconds: i64,
    /// Seconds the entry is past its TTL; `0` while it is still fresh (never
    /// negative). Populated on read.
    pub stale_seconds: i64,
    /// True if the scan was truncated because the prefix contained more than
    /// `MAX_SCAN_OBJECTS` objects. The totals represent a lower bound.
    #[serde(default)]
    pub truncated: bool,
    /// True when the original size of some objects is not known to this
    /// proxy (a delta, or an encrypted object, that no request through this
    /// proxy has read or written since it started), so those objects count
    /// their stored size instead: smaller for a delta, slightly larger for an
    /// encrypted object. `total_size` is then approximate. The scan never
    /// sends a metadata request per object to find out.
    #[serde(default)]
    pub sizes_estimated: bool,
}

/// Size and object count for an immediate child prefix.
#[derive(Clone, Default, Debug, PartialEq, Serialize)]
pub struct ChildUsage {
    /// Logical (original) bytes.
    pub size: u64,
    /// Bytes stored on the backend, delta baselines included.
    pub stored_size: u64,
    pub objects: u64,
    /// Same meaning as [`UsageEntry::sizes_estimated`], for this child.
    pub sizes_estimated: bool,
}

/// Totals of one scan, before they become a cache entry. Pure output of
/// [`aggregate_usage`].
#[derive(Default, Debug, PartialEq)]
pub(crate) struct UsageTotals {
    pub total_size: u64,
    pub stored_size: u64,
    pub total_objects: u64,
    pub sizes_estimated: bool,
    pub children: HashMap<String, ChildUsage>,
}

/// The immediate child folder of `prefix` that `key` lives in, or `None`
/// when `key` sits directly under `prefix` (or outside it).
fn child_prefix_of(prefix: &str, key: &str) -> Option<String> {
    let relative = key.strip_prefix(prefix)?;
    let slash = relative.find('/')?;
    Some(format!("{}{}/", prefix, &relative[..slash]))
}

/// Fold listed objects and delta baselines into logical + stored totals,
/// grouped by immediate child prefix. Pure; unit-tested.
///
/// * `stored[i]` is the stored size of `objects[i]` as LISTED, taken before
///   the listing-size cache replaced `file_size` with the logical size (for
///   an encrypted object `stored_size()` would then report the plaintext).
/// * `sizes[i]` says whether the logical size of `objects[i]` is known. An
///   object whose size is not known counts its stored size for both totals
///   and sets `sizes_estimated` (on the total and on its child).
/// * A baseline (`(stored key, stored size)`, e.g. `fw/v1/reference.bin`) is
///   not a user object: it adds stored bytes, but no logical bytes and no
///   object. Objects and baselines follow the SAME prefix rule (plain string
///   prefix, like the listing that produced both).
/// * Additions saturate: a corrupt size near `u64::MAX` must not wrap a total
///   to a tiny value (the quota fallback compares against it).
pub(crate) fn aggregate_usage(
    prefix: &str,
    objects: &[(String, crate::types::FileMetadata)],
    stored: &[u64],
    sizes: &[ListedSize],
    baselines: &[(String, u64)],
) -> UsageTotals {
    let mut t = UsageTotals::default();
    for (i, (key, meta)) in objects.iter().enumerate() {
        if !key.starts_with(prefix) {
            continue;
        }
        let stored = stored.get(i).copied().unwrap_or_else(|| meta.stored_size());
        // A missing entry (callers pass one per object) is treated as unknown.
        let known = sizes.get(i).is_some_and(|s| s.is_known());
        let logical = if known { meta.file_size } else { stored };
        t.total_size = t.total_size.saturating_add(logical);
        t.stored_size = t.stored_size.saturating_add(stored);
        t.total_objects = t.total_objects.saturating_add(1);
        t.sizes_estimated |= !known;
        if let Some(child) = child_prefix_of(prefix, key) {
            let c = t.children.entry(child).or_default();
            c.size = c.size.saturating_add(logical);
            c.stored_size = c.stored_size.saturating_add(stored);
            c.objects = c.objects.saturating_add(1);
            c.sizes_estimated |= !known;
        }
    }
    for (key, size) in baselines {
        if !key.starts_with(prefix) {
            continue;
        }
        t.stored_size = t.stored_size.saturating_add(*size);
        // `fw/reference.bin` under `fw/` belongs to the scanned folder itself,
        // which the total already counts; it forms no child.
        if let Some(child) = child_prefix_of(prefix, key) {
            let c = t.children.entry(child).or_default();
            c.stored_size = c.stored_size.saturating_add(*size);
        }
    }
    t
}

/// Seconds past the TTL for an entry of age `age`: `0` while fresh.
fn stale_seconds_for(age: i64, ttl: i64) -> i64 {
    (age - ttl).max(0)
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
    /// `age_seconds` / `stale_seconds` are filled in from the entry's age.
    pub fn get(&self, bucket: &str, prefix: &str) -> Option<UsageEntry> {
        let key = Self::cache_key(bucket, prefix);
        let cache = self.cache.read();
        if let Some(entry) = cache.get(&key) {
            let age = Utc::now()
                .signed_duration_since(entry.computed_at)
                .num_seconds();
            let mut result = entry.clone();
            result.age_seconds = age.max(0);
            result.stale_seconds = stale_seconds_for(age, cache_ttl_secs());
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
    ///
    /// One lite listing of the prefix and nothing else: no request per
    /// object, no request per folder. The listing carries the baselines too.
    /// Logical sizes come from the listing (filesystem: exact, from the
    /// xattrs it reads anyway) or from the listing-size cache (S3); an object
    /// neither knows counts its stored size and marks the result estimated.
    async fn do_scan(
        s3_state: &AppState,
        bucket: &str,
        prefix: &str,
    ) -> Result<UsageEntry, String> {
        let engine = s3_state.engine.load();
        let listing = engine
            .storage()
            .bulk_list_objects_with_baselines(bucket, prefix)
            .await
            .map_err(|e| format!("bulk_list_objects failed: {e}"))?;
        let mut objects = listing.objects;

        let truncated = objects.len() > MAX_SCAN_OBJECTS;
        if truncated {
            warn!(
                bucket = %bucket,
                prefix = %prefix,
                total = objects.len(),
                limit = MAX_SCAN_OBJECTS,
                "Scan truncated: prefix contains more objects than MAX_SCAN_OBJECTS"
            );
            objects.truncate(MAX_SCAN_OBJECTS);
        }
        // Stored sizes as listed, before the cache swaps in logical sizes.
        let stored: Vec<u64> = objects.iter().map(|(_, m)| m.stored_size()).collect();
        let sizes = engine
            .storage()
            .resolve_listed_sizes(bucket, &mut objects, false)
            .await;

        let totals = aggregate_usage(prefix, &objects, &stored, &sizes, &listing.baselines);
        Ok(UsageEntry {
            prefix: prefix.to_string(),
            bucket: bucket.to_string(),
            total_size: totals.total_size,
            stored_size: totals.stored_size,
            total_objects: totals.total_objects,
            children: totals.children,
            computed_at: Utc::now(),
            age_seconds: 0,
            stale_seconds: 0,
            truncated,
            sizes_estimated: totals.sizes_estimated,
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
            stored_size: size,
            total_objects: objects,
            children: HashMap::new(),
            computed_at: Utc::now(),
            age_seconds: 0,
            stale_seconds: 0,
            truncated: false,
            sizes_estimated: false,
        }
    }

    #[test]
    fn stale_seconds_is_zero_while_fresh_never_negative() {
        // Issue #92: a fresh entry reported `stale_seconds: -290`.
        assert_eq!(stale_seconds_for(10, 300), 0);
        assert_eq!(stale_seconds_for(300, 300), 0);
        assert_eq!(stale_seconds_for(301, 300), 1);
        let scanner = UsageScanner::new();
        UsageScanner::insert_with_eviction(
            &scanner.cache,
            "b/".to_string(),
            make_entry("b", "", 1, 1),
        );
        let r = scanner.get("b", "").unwrap();
        assert_eq!(r.stale_seconds, 0);
        assert!(r.age_seconds >= 0);
    }

    use crate::types::{FileMetadata, StorageInfo};

    fn delta(key: &str, original: u64, delta_size: u64) -> (String, FileMetadata) {
        let mut m = FileMetadata::fallback(
            key.rsplit('/').next().unwrap().to_string(),
            original,
            "etag".into(),
            Utc::now(),
            None,
            StorageInfo::delta_stub(delta_size),
        );
        // A resolved delta has its reference hash.
        if let StorageInfo::Delta { ref_sha256, .. } = &mut m.storage_info {
            *ref_sha256 = "abc".into();
        }
        (key.to_string(), m)
    }

    fn stub(key: &str, delta_size: u64) -> (String, FileMetadata) {
        let m = FileMetadata::fallback(
            key.rsplit('/').next().unwrap().to_string(),
            delta_size,
            "etag".into(),
            Utc::now(),
            None,
            StorageInfo::delta_stub(delta_size),
        );
        (key.to_string(), m)
    }

    fn plain(key: &str, size: u64) -> (String, FileMetadata) {
        let m = FileMetadata::fallback(
            key.rsplit('/').next().unwrap().to_string(),
            size,
            "etag".into(),
            Utc::now(),
            None,
            StorageInfo::Passthrough,
        );
        (key.to_string(), m)
    }

    fn known(objects: &[(String, FileMetadata)]) -> Vec<ListedSize> {
        objects
            .iter()
            .map(|(_, m)| {
                if m.is_unresolved_delta_stub() {
                    ListedSize::StoredOnly
                } else {
                    ListedSize::Listed
                }
            })
            .collect()
    }

    fn listed(objects: &[(String, FileMetadata)]) -> Vec<u64> {
        objects.iter().map(|(_, m)| m.stored_size()).collect()
    }

    /// Round-2 review: on a cache hit an encrypted object's `file_size`
    /// becomes the plaintext, so `stored_size()` would count plaintext as
    /// stored bytes. The listed (ciphertext) size is used instead.
    #[test]
    fn stored_bytes_are_the_listed_ones_even_after_a_cache_hit() {
        let mut enc = plain("e/a.bin", 1_000);
        enc.1.file_size = 972; // plaintext, set by the listing-size cache
        let t = aggregate_usage("", &[enc], &[1_000], &[ListedSize::Cached], &[]);
        assert_eq!(t.total_size, 972);
        assert_eq!(t.stored_size, 1_000);
        assert!(!t.sizes_estimated);
    }

    fn child(size: u64, stored_size: u64, objects: u64, sizes_estimated: bool) -> ChildUsage {
        ChildUsage {
            size,
            stored_size,
            objects,
            sizes_estimated,
        }
    }

    #[test]
    fn folder_size_is_logical_and_stored_includes_baselines() {
        // Issue #92: `firmware/` showed 230 B (the deltas) while the bucket
        // held 9 objects of about 3 MB each.
        let objects = vec![
            delta("firmware/v1/fw.tar", 3_000_000, 46),
            delta("firmware/v2/fw.tar", 3_100_000, 28_000),
            plain("firmware/README.md", 36),
            plain("top.txt", 10),
        ];
        let sizes = known(&objects);
        let refs = vec![("firmware/v1/reference.bin".to_string(), 3_000_000)];
        let t = aggregate_usage("", &objects, &listed(&objects), &sizes, &refs);
        assert_eq!(t.total_size, 3_000_000 + 3_100_000 + 36 + 10);
        assert_eq!(t.stored_size, 46 + 28_000 + 36 + 10 + 3_000_000);
        assert_eq!(t.total_objects, 4);
        assert!(!t.sizes_estimated);
        assert_eq!(
            t.children.get("firmware/"),
            Some(&child(
                3_000_000 + 3_100_000 + 36,
                46 + 28_000 + 36 + 3_000_000,
                3,
                false
            ))
        );
        assert_eq!(t.children.len(), 1, "top-level file forms no child");

        let t = aggregate_usage("firmware/", &objects, &listed(&objects), &sizes, &refs);
        assert_eq!(t.total_size, 3_000_000 + 3_100_000 + 36);
        assert_eq!(t.total_objects, 3);
        assert_eq!(
            t.children.get("firmware/v1/"),
            Some(&child(3_000_000, 46 + 3_000_000, 1, false))
        );
        assert_eq!(
            t.children.get("firmware/v2/"),
            Some(&child(3_100_000, 28_000, 1, false))
        );
    }

    #[test]
    fn baseline_of_the_scanned_folder_is_not_a_child() {
        let objects = vec![delta("fw/a.tar", 100, 5)];
        let refs = vec![("fw/reference.bin".to_string(), 100)];
        let t = aggregate_usage("fw/", &objects, &listed(&objects), &known(&objects), &refs);
        assert_eq!(t.stored_size, 105);
        assert!(t.children.is_empty(), "{:?}", t.children);
        let objects = vec![delta("a.tar", 100, 5)];
        let refs = vec![("reference.bin".to_string(), 100)];
        let t = aggregate_usage("", &objects, &listed(&objects), &known(&objects), &refs);
        assert_eq!(t.stored_size, 105);
        assert!(t.children.is_empty());
    }

    /// Review finding (round 2): baselines used a folder rule while objects
    /// used a string-prefix rule. One rule now: the prefix `fw` (no slash)
    /// covers `fw/…` and `fw2/…` for objects AND baselines alike.
    #[test]
    fn objects_and_baselines_follow_the_same_prefix_rule() {
        let objects = vec![delta("fw/a.tar", 100, 5), delta("fw2/b.tar", 200, 7)];
        let refs = vec![
            ("fw/reference.bin".to_string(), 100),
            ("fw2/reference.bin".to_string(), 200),
            ("other/reference.bin".to_string(), 999),
        ];
        let t = aggregate_usage("fw", &objects, &listed(&objects), &known(&objects), &refs);
        assert_eq!(t.total_size, 300);
        assert_eq!(t.stored_size, 5 + 7 + 100 + 200);
    }

    #[test]
    fn unknown_sizes_count_stored_bytes_and_flag_the_estimate() {
        let objects = vec![stub("d/a.tar", 46), delta("e/b.tar", 1000, 9)];
        let t = aggregate_usage("", &objects, &listed(&objects), &known(&objects), &[]);
        assert_eq!(t.total_size, 46 + 1000);
        assert!(t.sizes_estimated);
        assert!(t.children["d/"].sizes_estimated);
        assert!(
            !t.children["e/"].sizes_estimated,
            "only the folder with the unknown size"
        );
        // An object the cache resolved counts its logical size.
        let mut resolved = stub("d/a.tar", 46);
        resolved.1.file_size = 3_000;
        let t = aggregate_usage("", &[resolved], &[46], &[ListedSize::Cached], &[]);
        assert_eq!(t.total_size, 3_000);
        assert_eq!(t.stored_size, 46);
        assert!(!t.sizes_estimated);
    }

    #[test]
    fn sizes_saturate_instead_of_wrapping() {
        let objects = vec![plain("a", u64::MAX), plain("b", 5)];
        let t = aggregate_usage("", &objects, &listed(&objects), &known(&objects), &[]);
        assert_eq!(t.total_size, u64::MAX);
        assert_eq!(t.stored_size, u64::MAX);
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
