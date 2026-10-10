// SPDX-License-Identifier: BUSL-1.1

//! `GET /_/api/admin/deltaspace/savings?bucket=X&prefix=Y`
//!
//! Per-prefix savings totals for the SPA's "compression chip" + any
//! future visualisation that wants honest reference-aware numbers
//! without forcing the user to trigger a full bucket scan.
//!
//! Why a dedicated endpoint vs. computing client-side: the SPA can't
//! see `reference.bin` files (the engine hides them from list_objects
//! by design), so any client-side aggregator undercounts stored bytes
//! by one reference per deltaspace. Centralising the math here closes
//! that gap once for every consumer.
//!
//! Cost model: ONE lite listing of the prefix ([`scan_totals`]), paged by
//! 1000 keys and capped at [`MAX_LISTING_KEYS`] listed keys. The logical
//! sizes come from the listing-size cache and the listing facts, the
//! `reference.bin` baselines from the same pages: no HEAD, no second
//! listing. A size neither knows counts its stored size and marks the
//! answer `estimated`. Result is cached for 5 min per `(bucket, prefix)`
//! via [`moka`]'s coalescing `try_get_with`: concurrent misses for the
//! same key share one in-flight computation. On a prefix with more keys
//! than the cap the response carries `truncated: true` and the totals are
//! a lower bound; the operator-facing path for that is the bucket-wide
//! scan in `bucket_scan.rs`.
//!
//! HA caveat: the cache is per-instance. After a PUT routed to
//! instance A, instance B can serve pre-PUT savings up to 5 min later.
//! That window is intentional: the chip is a fingerprint, not an
//! invoice. Operators wanting cross-instance freshness invoke
//! `/_/api/admin/diagnostics/scan/start` which writes to a shared
//! disk-cached `ScanResult`.

use super::path_guard::{AdminBucket, AdminObjectPath};
use crate::api::admin::extract::AdminQuery;
use crate::api::admin::{AdminError, JsonError};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, Utc};
use moka::future::Cache;
use serde::{Deserialize, Serialize};

use crate::api::handlers::AppState;
use crate::deltaglider::{DynEngine, SavingsTotals};
use crate::storage::StorageBackend as _;
use crate::types::{FileMetadata, StorageInfo};

/// Max keys the chip lists before it stops with `truncated: true`. The
/// per-bucket scan in `bucket_scan.rs` is the path for huge prefixes.
const MAX_LISTING_KEYS: usize = 100_000;

/// In-memory cache TTL for savings responses. The UI asks once per folder
/// view (not on its 60 s refresh); a re-scan of a large folder costs
/// minutes of LIST requests on a slow backend, so the answer lives longer
/// than any refresh tick.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Keys listed per page of a totals scan (one LIST request on S3).
const PAGE_KEYS: usize = 1000;

/// Maximum cached entries. Sized generously — each entry is small
/// (~120 bytes) and the cost of a miss is the same paginated scan
/// either way. Beyond this moka does true TinyLFU eviction.
const CACHE_MAX_ENTRIES: u64 = 4096;

#[derive(Deserialize)]
pub struct SavingsQuery {
    pub bucket: AdminBucket,
    /// Default empty = whole bucket (same shape as the bucket scan).
    #[serde(default)]
    pub prefix: AdminObjectPath,
}

#[derive(Serialize, Clone)]
pub struct SavingsResponse {
    pub bucket: String,
    pub prefix: String,
    pub totals: SavingsTotals,
    /// Computed savings percentage 0..=99.99, or null when there's
    /// nothing under the prefix yet (avoids the UI showing "0%" for an
    /// empty browse).
    pub savings_percentage: Option<f64>,
    /// True when the walk hit `MAX_LISTING_KEYS` OR counted
    /// `DGP_REFERENCE_SCAN_LIMIT` references. The UI shows a `+` suffix and
    /// a "scope truncated" tooltip; numbers are a strict lower bound.
    pub truncated: bool,
    /// True when the original size of some objects is not known to this
    /// proxy (no listing facts, and no request through this proxy read or
    /// wrote them since it started): they count their stored size, so the
    /// original bytes and the savings are approximate. The chip never sends
    /// a HEAD per object to find out.
    pub estimated: bool,
    /// UTC timestamp when this scan finished. The SPA renders a
    /// "Recomputed Xs ago" hint from it.
    pub computed_at: DateTime<Utc>,
}

/// Cache + coalescing harness for per-prefix savings responses.
///
/// Implementation: `moka::future::Cache` provides three things in one
/// data structure:
///   1. TTL + TinyLFU eviction (replaces the hand-rolled
///      `RwLock<HashMap>` + "drop oldest" comment that was actually
///      drop-arbitrary).
///   2. `try_get_with` coalescing: concurrent misses for the same key
///      share ONE in-flight future. Closes the thundering-herd window
///      where N tabs hitting a cold prefix all fire N paginated
///      scans.
///   3. Lock-free reads on cache hit.
///
/// The `Arc<SavingsResponse>` value is shared by clone — cheap because
/// the inner struct is ~120 bytes and clone is just an Arc bump.
pub struct SavingsCache {
    inner: Cache<String, Arc<SavingsResponse>>,
}

impl SavingsCache {
    pub fn new() -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(CACHE_MAX_ENTRIES)
                .time_to_live(CACHE_TTL)
                .build(),
        }
    }

    fn cache_key(bucket: &str, prefix: &str) -> String {
        format!("{}\x00{}", bucket, prefix)
    }

    /// Get-or-compute with single-flight coalescing.
    ///
    /// If a value is cached and fresh, returns it. If not, runs `init`
    /// — but if another caller is already running `init` for the same
    /// key, both share that one future. Errors propagate to ALL
    /// awaiters of that key (moka semantics): if the first caller's
    /// compute fails, subsequent calls within the same await window
    /// see the same error, and the cache stays empty.
    pub async fn get_or_compute<F, Fut>(
        &self,
        bucket: &str,
        prefix: &str,
        init: F,
    ) -> Result<Arc<SavingsResponse>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<SavingsResponse, String>>,
    {
        let key = Self::cache_key(bucket, prefix);
        // moka's try_get_with takes `Future<Output = Result<V, E>>`
        // and stores ONLY the success value; on error it returns the
        // error wrapped in `Arc<E>` and does not cache. That's exactly
        // the semantics we want: a transient backend failure should
        // not poison the cache for the TTL.
        self.inner
            .try_get_with(key, async move {
                let v = init().await?;
                Ok::<_, String>(Arc::new(v))
            })
            .await
            .map_err(|arc_err| (*arc_err).clone())
    }

    /// Drop a single entry. Wired up if we ever add write-path
    /// invalidation hooks; not exercised yet (the TTL is considered
    /// acceptable lag for the savings display).
    #[allow(dead_code)]
    pub async fn invalidate(&self, bucket: &str, prefix: &str) {
        self.inner
            .invalidate(&Self::cache_key(bucket, prefix))
            .await;
    }
}

impl Default for SavingsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// `GET /_/api/admin/deltaspace/savings?bucket=X&prefix=Y`
pub async fn get_savings(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    AdminQuery(q): AdminQuery<SavingsQuery>,
) -> Result<Json<SavingsResponse>, AdminError<JsonError>> {
    // `AdminBucket` refuses an empty name at extraction (400
    // `invalid_bucket`), so the bucket is never empty here.
    let bucket = q.bucket.admit(&state)?;
    let s3_state = state.s3_state.clone();
    let bucket_for_compute = bucket.clone();
    let prefix_for_compute = q.prefix.clone();

    let arc = state
        .savings_cache
        .get_or_compute(&bucket, &q.prefix, move || async move {
            compute_savings(&s3_state, &bucket_for_compute, &prefix_for_compute).await
        })
        .await
        .map_err(AdminError::internal)?;
    // moka returns `Arc<SavingsResponse>`. Serde will follow
    // the Arc transparently via the inner Serialize impl.
    Ok(Json((*arc).clone()))
}

/// How a [`scan_totals`] walk ended early.
pub(crate) enum TotalsScanError {
    Cancelled,
    Failed(String),
}

/// Options for [`scan_totals`].
pub(crate) struct TotalsScanOpts<'a> {
    pub prefix: &'a str,
    /// Stop after this many LISTED keys (objects, baselines and folder
    /// markers; `truncated` = true when more remain). It bounds the LIST
    /// requests and the memory of the walk, not only its result.
    pub key_cap: Option<usize>,
    /// Count at most this many references (`truncated` = true past it).
    pub ref_limit: Option<usize>,
    /// HEAD the objects whose logical size neither the listing-size cache
    /// nor the listing facts know (and only those). Without it they count
    /// their stored size and the scan is `estimated`.
    pub head_unknown: bool,
    pub cancel: Option<&'a tokio_util::sync::CancellationToken>,
}

/// The outcome of a [`scan_totals`] walk.
pub(crate) struct TotalsScan {
    pub totals: SavingsTotals,
    /// The walk stopped at `key_cap` or `ref_limit`: a lower bound.
    pub truncated: bool,
    /// Some objects count their stored size: their logical size was not
    /// known (and a HEAD, when asked for, did not answer). Such totals never
    /// replace the usage counter.
    pub estimated: bool,
}

/// Listed objects: `(user key, metadata)`.
type Listed = Vec<(String, FileMetadata)>;

/// One page of a totals walk (see [`list_totals_page`]).
struct TotalsPage {
    objects: Listed,
    baselines: Vec<(String, u64)>,
    /// Objects whose logical size is still unknown.
    unknown_sizes: usize,
    next_start_after: Option<String>,
}

/// One page of a lite, recursive listing of `prefix`: at most `max_keys`
/// keys after `start_after`, the logical sizes from the listing-size cache
/// and the listing facts, and the `reference.bin` baselines the same LIST
/// requests returned. `head_unknown` HEADs the objects whose size is still
/// unknown (never one whose size is known).
async fn list_totals_page(
    engine: &DynEngine,
    bucket: &str,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
    head_unknown: bool,
) -> Result<TotalsPage, String> {
    let storage = engine.storage();
    let listing = storage
        .bulk_list_objects_with_baselines(bucket, prefix, start_after, Some(max_keys))
        .await
        .map_err(|e| e.to_string())?;
    let mut objects = listing.objects;
    let sizes = if objects.is_empty() {
        Vec::new()
    } else {
        storage
            .resolve_listed_sizes(bucket, &mut objects, false)
            .await
    };
    let mut unknown: Vec<usize> = (0..objects.len())
        .filter(|&i| sizes.get(i).is_some_and(|s| !s.is_known()))
        .collect();
    if head_unknown && !unknown.is_empty() {
        let misses = unknown.iter().map(|&i| objects[i].clone()).collect();
        let enriched = storage
            .enrich_list_metadata(bucket, misses)
            .await
            .map_err(|e| e.to_string())?;
        for (&i, (_, meta)) in unknown.iter().zip(enriched) {
            objects[i].1 = meta;
        }
        // A HEAD that failed (or a sweep stopped by throttling) leaves the
        // listing stub: still unknown.
        unknown.retain(|&i| objects[i].1.is_unresolved_delta_stub());
    }
    Ok(TotalsPage {
        objects,
        baselines: listing.baselines,
        unknown_sizes: unknown.len(),
        next_start_after: listing.next_start_after,
    })
}

/// The metadata of a listed `reference.bin` for [`SavingsTotals`]: stored
/// bytes, no user-visible object.
pub(crate) fn reference_metadata(stored_key: &str, stored_size: u64) -> FileMetadata {
    FileMetadata::fallback(
        stored_key
            .rsplit('/')
            .next()
            .unwrap_or(stored_key)
            .to_string(),
        stored_size,
        String::new(),
        Utc::now(),
        None,
        StorageInfo::Reference {
            source_name: String::new(),
        },
    )
}

/// Entries a page must hold back for the next one: `k` and `k.delta` (one
/// key stored in both forms during an overwrite) count once, and they can
/// sit on two pages. `k.delta` sorts after `k`, and between them sorts only
/// what extends `k`, so a pair is split at a page boundary only when `k` is
/// a prefix of the page's last key (`next_start_after`).
fn split_at_page_end(objects: Listed, page_end: Option<&str>) -> (Listed, Listed) {
    match page_end {
        Some(end) => objects
            .into_iter()
            .partition(|(key, _)| !(end.starts_with(key.as_str()) && end != key)),
        None => (objects, Vec::new()),
    }
}

/// THE savings scan: ONE lite listing of the prefix, paged by
/// [`PAGE_KEYS`] keys, folds every user-visible object and every
/// `reference.bin` baseline (hidden from LIST but real stored bytes) into
/// `SavingsTotals`. No HEAD per object: the logical sizes come from the
/// listing-size cache and the listing facts, and `head_unknown` HEADs only
/// the objects whose size neither knows. `on_page(totals, pages_done,
/// has_more)` runs after each page. Shared by the savings chip, the
/// dashboard scan and the usage Refresh. The engine is re-loaded per page,
/// so a config reload mid-scan uses the new engine.
pub(crate) async fn scan_totals(
    s3_state: &Arc<AppState>,
    bucket: &str,
    opts: TotalsScanOpts<'_>,
    mut on_page: impl FnMut(&SavingsTotals, u32, bool),
) -> Result<TotalsScan, TotalsScanError> {
    crate::types::ObjectKey::validate_prefix(opts.prefix)
        .map_err(|e| TotalsScanError::Failed(e.to_string()))?;
    let cancelled = || opts.cancel.is_some_and(|c| c.is_cancelled());
    let mut totals = SavingsTotals::default();
    let mut listed: usize = 0;
    let mut references: usize = 0;
    let mut truncated = false;
    let mut estimated = false;
    let mut start_after: Option<String> = None;
    let mut held: Listed = Vec::new();
    let mut pages_done: u32 = 0;
    loop {
        if cancelled() {
            return Err(TotalsScanError::Cancelled);
        }
        let engine = s3_state.engine.load_full();
        // One key past the cap tells whether the scope goes on.
        let window = opts
            .key_cap
            .map_or(PAGE_KEYS, |cap| (cap + 1 - listed.min(cap)).min(PAGE_KEYS));
        let list = list_totals_page(
            &engine,
            bucket,
            opts.prefix,
            start_after.as_deref(),
            window,
            opts.head_unknown,
        );
        let page = match opts.cancel {
            Some(cancel) => tokio::select! {
                _ = cancel.cancelled() => return Err(TotalsScanError::Cancelled),
                r = list => r,
            },
            None => list.await,
        }
        .map_err(TotalsScanError::Failed)?;
        listed += page.objects.len() + page.baselines.len();
        estimated |= page.unknown_sizes > 0;
        let mut objects = std::mem::take(&mut held);
        objects.extend(page.objects);
        let (counted, next_held) = split_at_page_end(
            crate::types::dedup_keep_latest(objects),
            page.next_start_after.as_deref(),
        );
        held = next_held;
        for (_, meta) in &counted {
            totals.accumulate(meta);
        }
        for (key, size) in &page.baselines {
            if opts.ref_limit.is_some_and(|limit| references >= limit) {
                truncated = true;
                continue;
            }
            references += 1;
            totals.accumulate(&reference_metadata(key, *size));
        }
        pages_done += 1;
        let mut has_more = page.next_start_after.is_some();
        if has_more && opts.key_cap.is_some_and(|cap| listed > cap) {
            truncated = true;
            has_more = false;
        }
        on_page(&totals, pages_done, has_more);
        if !has_more {
            break;
        }
        start_after = page.next_start_after;
    }
    for (_, meta) in &held {
        totals.accumulate(meta);
    }
    if cancelled() {
        return Err(TotalsScanError::Cancelled);
    }
    if truncated {
        tracing::info!(
            "savings scan of {bucket}/{} stopped at its cap; the totals are a lower bound",
            opts.prefix
        );
    }
    Ok(TotalsScan {
        totals,
        truncated,
        estimated,
    })
}

/// The savings chip: a capped [`scan_totals`] with no HEAD at all. A
/// reference costs nothing extra (it is on the listed pages), but the chip
/// still counts at most `DGP_REFERENCE_SCAN_LIMIT` of them.
async fn compute_savings(
    s3_state: &Arc<AppState>,
    bucket: &str,
    prefix: &str,
) -> Result<SavingsResponse, String> {
    compute_savings_capped(s3_state, bucket, prefix, MAX_LISTING_KEYS).await
}

/// [`compute_savings`] with the listing cap as a parameter (tests lower it).
async fn compute_savings_capped(
    s3_state: &Arc<AppState>,
    bucket: &str,
    prefix: &str,
    cap: usize,
) -> Result<SavingsResponse, String> {
    let ref_limit = s3_state.engine.load().tuning().reference_scan_limit;
    let scan = scan_totals(
        s3_state,
        bucket,
        TotalsScanOpts {
            prefix,
            key_cap: Some(cap),
            ref_limit: Some(ref_limit),
            head_unknown: false,
            cancel: None,
        },
        |_, _, _| {},
    )
    .await
    .map_err(|e| match e {
        TotalsScanError::Failed(msg) => msg,
        TotalsScanError::Cancelled => "scan cancelled".to_string(),
    })?;

    let savings_percentage = scan.totals.savings_percentage();
    Ok(SavingsResponse {
        bucket: bucket.to_string(),
        prefix: prefix.to_string(),
        totals: scan.totals,
        savings_percentage,
        truncated: scan.truncated,
        estimated: scan.estimated,
        computed_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query extractor refuses an empty bucket, so the handler needs
    /// no empty-bucket branch of its own.
    #[test]
    fn savings_query_refuses_an_empty_bucket() {
        let q: Result<SavingsQuery, _> = serde_json::from_str(r#"{"bucket":""}"#);
        let err = q.err().expect("empty bucket must not parse").to_string();
        assert!(err.contains("invalid bucket name"), "{err}");
        let ok: SavingsQuery = serde_json::from_str(r#"{"bucket":"releases"}"#).unwrap();
        assert_eq!(ok.prefix.as_str(), "");
    }

    use crate::usage_scanner::test_support::{
        fake_s3_engine, heads, put_raw, put_raw_many, scope_lists,
    };

    /// Cockroach scan (ui.md A, limits #5): the chip listed with
    /// `metadata=true`, so a cold metadata cache cost one HEAD per delta on
    /// every folder view (200 HEADs here).
    #[tokio::test]
    async fn the_savings_chip_sends_no_head_per_delta() {
        let (engine, fake, _) = fake_s3_engine().await;
        let keys = crate::deltaglider::store_deltas(&engine, "f", 200).await;
        for k in &keys {
            engine.invalidate_metadata_cache("b", k);
        }
        let state = AppState::for_tests(engine);
        fake.clear();
        let r = compute_savings_capped(&state, "b", "f/", MAX_LISTING_KEYS)
            .await
            .unwrap();
        let heads = heads(&fake);
        assert!(heads <= 1, "{heads} HEADs for 200 deltas and 1 reference");
        assert_eq!((r.totals.delta_count, r.totals.reference_count), (200, 1));
        assert!(!r.truncated);
        // This process wrote them: the listing-size cache knows their sizes.
        assert!(!r.estimated);
        assert_eq!(r.totals.original_bytes, 200 * 64 * 1024);
    }

    /// A delta whose original size no cache and no listing fact knows
    /// counts its stored size, and the chip says so instead of sending a
    /// HEAD.
    #[tokio::test]
    async fn a_delta_of_unknown_size_makes_the_chip_an_estimate() {
        let (engine, fake, endpoint) = fake_s3_engine().await;
        put_raw(&endpoint, "e/build.zip.delta", &[1u8; 100]).await;
        put_raw(&endpoint, "e/reference.bin", &[2u8; 1000]).await;
        let state = AppState::for_tests(engine);
        fake.clear();
        let r = compute_savings_capped(&state, "b", "e/", MAX_LISTING_KEYS)
            .await
            .unwrap();
        assert_eq!(heads(&fake), 0);
        assert!(r.estimated);
        assert_eq!((r.totals.delta_count, r.totals.reference_bytes), (1, 1000));
    }

    /// `k` and `k.delta` (one key in both forms during an overwrite) count
    /// once, also when the keys between them push `k.delta` onto the next
    /// listing page.
    #[tokio::test]
    async fn a_key_in_both_forms_counts_once_across_pages() {
        let (engine, _fake, endpoint) = fake_s3_engine().await;
        put_raw(&endpoint, "d/k.zip", b"plain").await;
        // `-` sorts before `.`: these come between `d/k.zip` and its delta.
        put_raw_many(&endpoint, (0..1000).map(|i| format!("d/k.zip-{i:04}"))).await;
        put_raw(&endpoint, "d/k.zip.delta", b"delta").await;
        let state = AppState::for_tests(engine);
        let opts = TotalsScanOpts {
            prefix: "d/",
            key_cap: None,
            ref_limit: None,
            head_unknown: false,
            cancel: None,
        };
        let mut pages = 0;
        let scan = scan_totals(&state, "b", opts, |_, n, _| pages = n)
            .await
            .unwrap_or_else(|_| panic!("scan failed"));
        assert_eq!(pages, 2, "the pair spans a page boundary");
        assert_eq!(scan.totals.user_visible_count(), 1001);
    }

    #[test]
    fn a_page_holds_back_the_keys_its_end_extends() {
        let meta = |k: &str| {
            crate::types::FileMetadata::fallback(
                k.into(),
                1,
                String::new(),
                Utc::now(),
                None,
                crate::types::StorageInfo::Passthrough,
            )
        };
        let page = ["a/w", "a/x", "a/x.zip"]
            .iter()
            .map(|k| (k.to_string(), meta(k)))
            .collect();
        let (counted, held) = split_at_page_end(page, Some("a/x.zip.delta"));
        let keys = |v: Vec<(String, crate::types::FileMetadata)>| {
            v.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        };
        assert_eq!(keys(counted), ["a/w"]);
        assert_eq!(keys(held), ["a/x", "a/x.zip"]);
        let (counted, held) = split_at_page_end(vec![("a/x".into(), meta("x"))], None);
        assert_eq!((counted.len(), held.len()), (1, 0));
    }

    /// Cockroach scan (limits #4): the cap counted user-visible objects, and
    /// the reference walk then listed the whole scope again with no cap.
    #[tokio::test]
    async fn the_savings_walk_lists_no_key_past_its_cap() {
        let (engine, fake, endpoint) = fake_s3_engine().await;
        put_raw_many(
            &endpoint,
            (0..2500).map(|i| format!("s/d{}/img-{i:05}.jpg", i % 5)),
        )
        .await;
        put_raw(&endpoint, "s/d4/reference.bin", b"baseline").await;
        let state = AppState::for_tests(engine);
        fake.clear();
        let r = compute_savings_capped(&state, "b", "s/", 100)
            .await
            .unwrap();
        let lists = scope_lists(&fake);
        assert_eq!(
            lists.len(),
            1,
            "a cap of 100 keys sent {} LIST requests: {lists:?}",
            lists.len()
        );
        assert!(r.truncated);
    }
}
