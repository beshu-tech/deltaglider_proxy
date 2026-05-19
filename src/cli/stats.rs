// SPDX-License-Identifier: GPL-3.0-only

//! `deltaglider_proxy s3 stats s3://bucket [--quick|--sampled|--detailed]
//!  [--refresh] [--no-cache] [--json]`
//!
//! Bucket-scoped compression metrics. Three accuracy/runtime tiers:
//!
//! - **quick** (default): one LIST pass, no HEAD. Compression ratios for
//!   delta files come from whatever the storage layer already returned
//!   (cached original sizes via [`crate::deltaglider::engine`]'s metadata
//!   cache, or stored sizes for cold objects). Sub-second on large buckets.
//! - **sampled**: LIST + one HEAD per deltaspace, project the metadata
//!   onto siblings. Catches buckets where the metadata cache is cold but
//!   still cheap (~5-15s for ten-thousand-object buckets).
//! - **detailed**: LIST with `metadata=true` — HEAD every object. Most
//!   accurate, slowest. This was the MVP's single mode.
//!
//! Results are cached at `s3://<bucket>/.deltaglider/stats_{mode}.json`
//! (one file per mode). Cache validation re-runs a cheap LIST and compares
//! `(object_count, compressed_size)` against the cached tuple; on drift,
//! it recomputes. Mutually compatible wire format with the Python toolchain
//! at the JSON-document level (`{ version, mode, computed_at, validation,
//! stats }`), though the `stats` payload uses our field names. `--refresh`
//! forces a recompute, `--no-cache` skips both read and write.

use crate::api::admin::{classify_deltaspace, Efficiency};
use crate::cli::aws_creds;
use crate::cli::config as cli_exit;
use crate::cli::engine_factory::{build_cli_engine, CliEngineOpts};
use crate::cli::ls::should_allow_local;
use crate::cli::s3_url::{is_s3_url, parse_s3_url};
use crate::deltaglider::DynEngine;
use crate::types::{FileMetadata, StorageInfo};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// CLI scan strategy. Three tiers of accuracy vs runtime; see module
/// docs for the trade-off table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Quick,
    Sampled,
    Detailed,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Quick => "quick",
            Mode::Sampled => "sampled",
            Mode::Detailed => "detailed",
        }
    }
}

/// Bucket statistics: object counts, original-vs-stored bytes,
/// savings %, and a per-deltaspace health roll-up.
#[derive(clap::Args, Debug, Clone)]
pub struct StatsArgs {
    /// S3 URL (`s3://bucket` — bucket-scoped only).
    #[arg(value_name = "S3_URL")]
    pub url: String,

    /// Quick scan: LIST only, no HEAD requests. Fast, may underreport
    /// savings on cold buckets. Default.
    #[arg(long, conflicts_with_all = ["sampled", "detailed"])]
    pub quick: bool,

    /// Sampled scan: HEAD one delta per deltaspace and project. Middle
    /// ground between quick and detailed.
    #[arg(long, conflicts_with_all = ["quick", "detailed"])]
    pub sampled: bool,

    /// Detailed scan: HEAD every object. Most accurate, slowest.
    #[arg(long, conflicts_with_all = ["quick", "sampled"])]
    pub detailed: bool,

    /// Force a cache recompute even if a valid cache file exists.
    #[arg(long, conflicts_with = "no_cache")]
    pub refresh: bool,

    /// Skip the on-bucket cache for both read and write.
    #[arg(long, conflicts_with = "refresh")]
    pub no_cache: bool,

    /// Emit the results as a single JSON object on stdout (no human
    /// preamble). Shape matches the admin `bucket-scan` endpoint's
    /// `ScanResult` so cross-tool downstream tooling can consume both.
    #[arg(long)]
    pub json: bool,

    /// S3 endpoint URL.
    #[arg(long, value_name = "URL")]
    pub endpoint_url: Option<String>,

    /// AWS region.
    #[arg(long, value_name = "NAME")]
    pub region: Option<String>,

    /// AWS profile.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Override `AWS_ACCESS_KEY_ID`.
    #[arg(long, value_name = "ID")]
    pub access_key_id: Option<String>,

    /// Override `AWS_SECRET_ACCESS_KEY`.
    #[arg(long, value_name = "KEY")]
    pub secret_access_key: Option<String>,

    /// Use path-style URLs (MinIO / LocalStack).
    #[arg(long)]
    pub force_path_style: bool,
}

impl StatsArgs {
    fn mode(&self) -> Mode {
        if self.detailed {
            Mode::Detailed
        } else if self.sampled {
            Mode::Sampled
        } else {
            // --quick is the default when no flag is given.
            Mode::Quick
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DeltaspaceHealth {
    pub excellent: u64,
    pub good: u64,
    pub fair: u64,
    pub poor: u64,
    pub no_reference: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StatsResult {
    pub bucket: String,
    pub total_objects: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    pub savings_percentage: f64,
    pub deltaspace_health: DeltaspaceHealth,
}

// Bumped from "1.0" → "2.0" when reference bytes started counting
// toward `total_stored_bytes`. Old cache entries undercount by the
// reference total and produce headline numbers that lie; drop them on
// load so the next CLI invocation rescans honestly.
const CACHE_VERSION: &str = "2.0";

/// On-bucket cache shape. Compatible with the Python toolchain at the
/// document level (`version`/`mode`/`computed_at`/`validation`/`stats`),
/// though the `stats` payload uses Rust field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheDoc {
    version: String,
    mode: Mode,
    computed_at: chrono::DateTime<Utc>,
    validation: Validation,
    stats: StatsResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Validation {
    object_count: u64,
    compressed_size: u64,
}

/// Pure accumulator. One pass through every `(key, metadata)`
/// the engine returns; the caller rolls deltaspace verdicts at the
/// end via `into_result`.
///
/// Savings math is delegated to [`crate::deltaglider::SavingsTotals`];
/// `StatsAcc` only owns the *per-deltaspace efficiency classification*
/// (the `spaces` map) on top of that. The CLI MUST feed both user-
/// visible objects (via `list_objects`) AND every deltaspace
/// `reference.bin` (via `engine.list_deltaspace_references`) into
/// `record`; the centralized totals module sums them honestly, so
/// "savings_percentage" is what you'd actually save by uninstalling
/// the proxy and stuffing the originals back on disk.
#[derive(Debug, Default)]
pub(crate) struct StatsAcc {
    pub totals: crate::deltaglider::SavingsTotals,
    /// deltaspace prefix → (reference_size, delta_size_list)
    pub spaces: HashMap<String, (Option<u64>, Vec<u64>)>,
}

impl StatsAcc {
    pub fn record(&mut self, key: &str, meta: &FileMetadata) {
        self.totals.accumulate(meta);

        let deltaspace = deltaspace_id_for_key(key);
        match &meta.storage_info {
            StorageInfo::Reference { .. } => {
                let entry = self.spaces.entry(deltaspace).or_default();
                entry.0 = Some(meta.file_size);
            }
            StorageInfo::Delta { delta_size, .. } => {
                let entry = self.spaces.entry(deltaspace).or_default();
                entry.1.push(*delta_size);
            }
            StorageInfo::Passthrough => {
                // Doesn't contribute to a deltaspace verdict. Still
                // counted in `totals` above.
            }
        }
    }

    pub fn into_result(self, bucket: &str) -> StatsResult {
        let mut health = DeltaspaceHealth::default();
        // `min_deltas = 1` matches the admin-API default — a single
        // delta is enough signal for the CLI's bucket-wide roll-up.
        for (ref_size, deltas) in self.spaces.values() {
            if let Some(eff) = classify_deltaspace(*ref_size, deltas, 1) {
                match eff {
                    Efficiency::Excellent => health.excellent += 1,
                    Efficiency::Good => health.good += 1,
                    Efficiency::Fair => health.fair += 1,
                    Efficiency::Poor => health.poor += 1,
                    Efficiency::NoReference => health.no_reference += 1,
                }
            }
        }
        StatsResult {
            bucket: bucket.to_string(),
            total_objects: self.totals.user_visible_count(),
            total_original_bytes: self.totals.original_bytes,
            total_stored_bytes: self.totals.stored_bytes,
            savings_percentage: self.totals.savings_percentage().unwrap_or(0.0),
            deltaspace_health: health,
        }
    }
}

/// Convenience: after a list_objects-driven scan, fold in every
/// `reference.bin` under the bucket so the totals reflect on-disk cost.
/// MUST be called before `into_result` for honest numbers.
async fn fold_in_references(
    acc: &mut StatsAcc,
    engine: &DynEngine,
    bucket: &str,
) -> Result<(), i32> {
    // limit=None: the CLI `s3 stats` command is the operator's "tell
    // me the real number" path — same intent as the admin dashboard
    // scan. The cap is for chip-style latency-sensitive consumers, not
    // here.
    let scan = engine
        .list_deltaspace_references(bucket, "", None)
        .await
        .map_err(|e| {
            eprintln!("warning: failed to enumerate references for {bucket}: {e}");
            cli_exit::EXIT_HTTP
        })?;
    for meta in &scan.references {
        // The reference key shape is `<prefix>reference.bin` — we use
        // the prefix as the deltaspace id elsewhere; here we just need
        // the per-object savings contribution, so any key in the right
        // deltaspace works. The metadata.storage_info already says
        // Reference, so the SavingsTotals accumulator handles the rest.
        acc.record("__internal/reference.bin", meta);
    }
    Ok(())
}

/// Pure: pick the deltaspace identifier from an object key. We use
/// the parent prefix (everything up to and including the last `/`);
/// a bare key (no slash) lives in the bucket's root deltaspace.
pub(crate) fn deltaspace_id_for_key(key: &str) -> String {
    match key.rfind('/') {
        Some(i) => key[..=i].to_string(),
        None => String::new(),
    }
}

/// Pure: stored-on-disk bytes for one object's `FileMetadata`.
///
/// Thin alias for [`FileMetadata::stored_size`]. Kept as a free
/// function only because the existing CLI tests (and the cache-
/// validation listing) refer to it by name; the math itself lives on
/// `FileMetadata` so a stored-size definition exists in exactly one
/// place.
pub(crate) fn stored_size_of(meta: &FileMetadata) -> u64 {
    meta.stored_size()
}

/// Pure: cache key for a given mode. Mirrors the Python toolchain so
/// caches written by either CLI are mutually readable at the doc level.
fn cache_key_for(mode: Mode) -> String {
    format!(".deltaglider/stats_{}.json", mode.as_str())
}

/// Pure: did the bucket's basic shape change since `cached` was written?
/// We compare `(object_count, compressed_size)` only — anything beyond
/// that pulls metadata back into the validation phase and defeats the
/// "cheap LIST to validate cache" property.
fn is_cache_valid(cached: &Validation, current: &Validation) -> bool {
    cached.object_count == current.object_count && cached.compressed_size == current.compressed_size
}

/// Pure: is this key one of our internal tooling artifacts that should
/// be excluded from stats? Currently just the `.deltaglider/` namespace
/// (config DB, stats caches, future bookkeeping). Filtering this here
/// keeps the cache from self-invalidating on every write.
fn is_internal_key(key: &str) -> bool {
    key.starts_with(".deltaglider/")
}

pub async fn run(args: StatsArgs) -> i32 {
    if !is_s3_url(&args.url) {
        eprintln!("error: expected an `s3://bucket` URL, got `{}`", args.url);
        return cli_exit::EXIT_USAGE;
    }
    let loc = match parse_s3_url(&args.url) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: bad S3 URL: {e}");
            return cli_exit::EXIT_PARSE;
        }
    };
    if !loc.key.is_empty() {
        eprintln!(
            "error: stats is bucket-scoped (no prefix); got s3://{}/{}",
            loc.bucket, loc.key
        );
        return cli_exit::EXIT_USAGE;
    }

    let creds = match aws_creds::resolve(aws_creds::CredsInputs {
        access_key_flag: args.access_key_id.as_deref(),
        secret_key_flag: args.secret_access_key.as_deref(),
        region_flag: args.region.as_deref(),
        profile_flag: args.profile.as_deref(),
        ..Default::default()
    }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return cli_exit::EXIT_AUTH;
        }
    };

    let opts = CliEngineOpts {
        endpoint: args.endpoint_url.clone(),
        region: creds.region.unwrap_or_else(|| "us-east-1".into()),
        force_path_style: args.force_path_style,
        access_key_id: creds.access_key_id,
        secret_access_key: creds.secret_access_key,
        max_delta_ratio: None,
        allow_local: should_allow_local(args.endpoint_url.as_deref()),
    };
    let engine = match build_cli_engine(opts).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: failed to initialise S3 client: {e}");
            return cli_exit::EXIT_HTTP;
        }
    };

    let mode = args.mode();
    let use_cache = !args.no_cache;

    let outcome = match compute_stats(&engine, &loc.bucket, mode, use_cache, args.refresh).await {
        Ok(o) => o,
        Err(code) => return code,
    };

    emit(&outcome.result, mode, outcome.cache_hit, args.json);
    cli_exit::EXIT_OK
}

struct ComputeOutcome {
    result: StatsResult,
    cache_hit: bool,
}

async fn compute_stats(
    engine: &DynEngine,
    bucket: &str,
    mode: Mode,
    use_cache: bool,
    refresh: bool,
) -> Result<ComputeOutcome, i32> {
    // Phase 1: cheap LIST to compute validation (count + total stored size).
    // For Quick mode this is also our only data source. We always run it
    // because the cache validation needs current numbers.
    let listing = list_for_validation(engine, bucket).await?;

    let validation = Validation {
        object_count: listing.entries.len() as u64,
        compressed_size: listing.total_stored_bytes,
    };

    // Phase 2: cache lookup. Skipped on --no-cache or --refresh.
    if use_cache && !refresh {
        if let Some(cached) = try_read_cache(engine, bucket, mode).await {
            if cached.version == CACHE_VERSION
                && cached.mode == mode
                && is_cache_valid(&cached.validation, &validation)
            {
                return Ok(ComputeOutcome {
                    result: cached.stats,
                    cache_hit: true,
                });
            }
        }
    }

    // Phase 3: compute fresh stats per mode. Every mode delegates the
    // savings math to `SavingsTotals` (see `StatsAcc::into_result`) and
    // folds `reference.bin` bytes into `total_stored_bytes` — without
    // that, the headline number lies by a full reference per
    // deltaspace.
    let result = match mode {
        Mode::Quick => quick_scan(engine, bucket, &listing).await?,
        Mode::Sampled => sampled_from_listing(engine, bucket, &listing).await?,
        Mode::Detailed => detailed_scan(engine, bucket).await?,
    };

    // Phase 4: persist the cache (best-effort — a write failure isn't
    // fatal because the next invocation will just recompute).
    if use_cache {
        let doc = CacheDoc {
            version: CACHE_VERSION.into(),
            mode,
            computed_at: Utc::now(),
            validation,
            stats: result.clone(),
        };
        if let Err(e) = write_cache(engine, bucket, mode, &doc).await {
            eprintln!("warning: failed to write stats cache: {e}");
        }
    }

    Ok(ComputeOutcome {
        result,
        cache_hit: false,
    })
}

/// LIST every (non-internal) object in the bucket, capturing the
/// `(key, file_size)` tuple and the bare `FileMetadata` shape so quick
/// mode can accumulate without HEAD-ing. Filters out `.deltaglider/`
/// entries so cache files don't poison the validation numbers.
struct BucketListing {
    entries: Vec<(String, FileMetadata)>,
    total_stored_bytes: u64,
}

async fn list_for_validation(engine: &DynEngine, bucket: &str) -> Result<BucketListing, i32> {
    let mut entries = Vec::new();
    let mut total_stored: u64 = 0;
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, "", None, 1000, continuation.as_deref(), false)
            .await
            .map_err(|e| {
                eprintln!("error: list_objects failed: {e}");
                cli_exit::EXIT_HTTP
            })?;
        for (key, meta) in page.objects {
            if is_internal_key(&key) {
                continue;
            }
            total_stored = total_stored.saturating_add(stored_size_of(&meta));
            entries.push((key, meta));
        }
        if !page.is_truncated {
            break;
        }
        continuation = page.next_continuation_token;
        if continuation.is_none() {
            break;
        }
    }
    Ok(BucketListing {
        entries,
        total_stored_bytes: total_stored,
    })
}

/// Quick mode: trust the listing's `FileMetadata` (which has cached
/// originals where available, stored sizes otherwise). The listing
/// itself hides references; the caller MUST fold them in afterward
/// via `fold_in_references` so `total_stored_bytes` reflects on-disk
/// cost honestly. We expose `quick_from_listing_unfolded` as a pure
/// helper for unit tests; `quick_scan` below is the real entry point.
fn quick_from_listing_unfolded(listing: &BucketListing) -> StatsAcc {
    let mut acc = StatsAcc::default();
    for (key, meta) in &listing.entries {
        acc.record(key, meta);
    }
    acc
}

async fn quick_scan(
    engine: &DynEngine,
    bucket: &str,
    listing: &BucketListing,
) -> Result<StatsResult, i32> {
    let mut acc = quick_from_listing_unfolded(listing);
    fold_in_references(&mut acc, engine, bucket).await?;
    Ok(acc.into_result(bucket))
}

/// Sampled mode: HEAD one object per deltaspace prefix, then project
/// the resulting metadata onto every sibling in the same prefix. Trade:
/// O(deltaspaces) HEADs instead of O(objects), with the assumption that
/// siblings share storage class and ratios within a prefix.
async fn sampled_from_listing(
    engine: &DynEngine,
    bucket: &str,
    listing: &BucketListing,
) -> Result<StatsResult, i32> {
    // Pick one key per deltaspace (first occurrence wins — stable enough
    // for sampling).
    let mut seen: HashSet<String> = HashSet::new();
    let mut samples: HashMap<String, String> = HashMap::new();
    for (key, _) in &listing.entries {
        let space = deltaspace_id_for_key(key);
        if seen.insert(space.clone()) {
            samples.insert(space, key.clone());
        }
    }

    // HEAD each sampled key. A miss is non-fatal — that deltaspace just
    // keeps its listing-derived metadata.
    let mut head_results: HashMap<String, FileMetadata> = HashMap::new();
    for (space, key) in &samples {
        match engine.head(bucket, key).await {
            Ok(m) => {
                head_results.insert(space.clone(), m);
            }
            Err(e) => {
                eprintln!("warning: HEAD {} failed during sampling: {}", key, e);
            }
        }
    }

    // Accumulate with projection: per-object, if HEAD succeeded for its
    // deltaspace and we don't have cached metadata for this specific key,
    // borrow the sampled metadata's storage_info and file_size.
    let mut acc = StatsAcc::default();
    for (key, listed_meta) in &listing.entries {
        let space = deltaspace_id_for_key(key);
        let projected = match head_results.get(&space) {
            Some(sample) if matches!(listed_meta.storage_info, StorageInfo::Passthrough) => {
                // Only project when the listing didn't already classify
                // this object as Reference/Delta — preserve any concrete
                // signal we already have.
                let mut m = listed_meta.clone();
                m.storage_info = sample.storage_info.clone();
                if listed_meta.file_size == 0 {
                    m.file_size = sample.file_size;
                }
                m
            }
            _ => listed_meta.clone(),
        };
        acc.record(key, &projected);
    }

    fold_in_references(&mut acc, engine, bucket).await?;
    Ok(acc.into_result(bucket))
}

/// Detailed mode: the MVP's original behavior. LIST with metadata=true
/// so every object's metadata is HEAD-ed (with cache de-dup).
async fn detailed_scan(engine: &DynEngine, bucket: &str) -> Result<StatsResult, i32> {
    let mut acc = StatsAcc::default();
    let mut continuation: Option<String> = None;
    loop {
        let page = engine
            .list_objects(bucket, "", None, 1000, continuation.as_deref(), true)
            .await
            .map_err(|e| {
                eprintln!("error: list_objects failed: {e}");
                cli_exit::EXIT_HTTP
            })?;
        for (key, meta) in &page.objects {
            if is_internal_key(key) {
                continue;
            }
            acc.record(key, meta);
        }
        if !page.is_truncated {
            break;
        }
        continuation = page.next_continuation_token;
        if continuation.is_none() {
            break;
        }
    }
    fold_in_references(&mut acc, engine, bucket).await?;
    Ok(acc.into_result(bucket))
}

async fn try_read_cache(engine: &DynEngine, bucket: &str, mode: Mode) -> Option<CacheDoc> {
    let key = cache_key_for(mode);
    let (bytes, _) = engine.retrieve(bucket, &key).await.ok()?;
    serde_json::from_slice::<CacheDoc>(&bytes).ok()
}

async fn write_cache(
    engine: &DynEngine,
    bucket: &str,
    mode: Mode,
    doc: &CacheDoc,
) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(doc).map_err(|e| format!("serialise cache doc: {e}"))?;
    let mut user_meta = HashMap::new();
    user_meta.insert("x-deltaglider-cache".to_string(), "true".to_string());
    // Tag with the same hint the proxy server already honours so the
    // codec never tries to delta-encode this JSON blob.
    user_meta.insert("dg-no-delta".to_string(), "true".to_string());
    engine
        .store(
            bucket,
            &cache_key_for(mode),
            &body,
            Some("application/json".into()),
            user_meta,
        )
        .await
        .map_err(|e| format!("store cache: {e}"))?;
    Ok(())
}

fn emit(result: &StatsResult, mode: Mode, cache_hit: bool, json: bool) {
    if json {
        match serde_json::to_string(result) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("error: serialising stats failed: {e}"),
        }
        return;
    }
    println!("Bucket:                 {}", result.bucket);
    println!(
        "Mode:                   {}{}",
        mode.as_str(),
        if cache_hit { " (cache hit)" } else { "" }
    );
    println!("Total Objects:          {}", result.total_objects);
    println!("Total Original Bytes:   {}", result.total_original_bytes);
    println!("Total Stored Bytes:     {}", result.total_stored_bytes);
    println!("Savings:                {:.2}%", result.savings_percentage);
    let h = &result.deltaspace_health;
    println!(
        "Deltaspace Health:      excellent={} good={} fair={} poor={} no_reference={}",
        h.excellent, h.good, h.fair, h.poor, h.no_reference
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileMetadata, StorageInfo};
    use chrono::Utc;

    fn meta(file_size: u64, info: StorageInfo) -> FileMetadata {
        FileMetadata {
            tool: "deltaglider/test".into(),
            original_name: "x.bin".into(),
            file_sha256: "0".into(),
            md5: "0".into(),
            file_size,
            multipart_etag: None,
            created_at: Utc::now(),
            content_type: None,
            user_metadata: Default::default(),
            storage_info: info,
        }
    }

    #[test]
    fn deltaspace_id_is_parent_prefix() {
        assert_eq!(deltaspace_id_for_key("releases/v1.zip"), "releases/");
        assert_eq!(deltaspace_id_for_key("a/b/c.zip"), "a/b/");
        assert_eq!(deltaspace_id_for_key("bare.zip"), "");
    }

    #[test]
    fn stored_size_picks_delta_size_for_delta_variant() {
        let m = meta(
            1024,
            StorageInfo::Delta {
                ref_path: "reference.bin".into(),
                ref_sha256: "abc".into(),
                delta_size: 64,
                delta_cmd: "xdelta3 …".into(),
            },
        );
        assert_eq!(stored_size_of(&m), 64);
    }

    #[test]
    fn stored_size_for_passthrough_is_file_size() {
        assert_eq!(stored_size_of(&meta(1024, StorageInfo::Passthrough)), 1024);
    }

    #[test]
    fn stored_size_for_reference_is_file_size() {
        let m = meta(
            1024,
            StorageInfo::Reference {
                source_name: "v0.zip".into(),
            },
        );
        assert_eq!(stored_size_of(&m), 1024);
    }

    #[test]
    fn acc_classifies_excellent_deltaspace() {
        let mut acc = StatsAcc::default();
        // Reference = 200 KiB, two deltas at 1 KiB each → median ratio 0.5%.
        acc.record(
            "releases/v0.zip",
            &meta(
                200_000,
                StorageInfo::Reference {
                    source_name: "v0.zip".into(),
                },
            ),
        );
        for i in 1..=2 {
            acc.record(
                &format!("releases/v{i}.zip"),
                &meta(
                    200_000,
                    StorageInfo::Delta {
                        ref_path: "reference.bin".into(),
                        ref_sha256: "abc".into(),
                        delta_size: 1_000,
                        delta_cmd: "xdelta3 …".into(),
                    },
                ),
            );
        }
        let r = acc.into_result("test");
        // 2 deltas — the reference is internal and not counted as a
        // user-visible object (previously it leaked into total_objects
        // and exaggerated user-facing counts).
        assert_eq!(r.total_objects, 2);
        assert_eq!(r.deltaspace_health.excellent, 1);
        // Stored = 200 KB (reference) + 2 × 1 KB (deltas) = 202 KB.
        // Original (user-visible) = 2 × 200 KB = 400 KB. Savings ≈ 49.5%.
        // The reference is REAL cost — without it, "savings" looks like
        // 99% but lies. This is the bug the centralization closes.
        assert!(
            (49.0..=51.0).contains(&r.savings_percentage),
            "expected ~50% (200KB ref + 2×1KB delta vs 2×200KB original); got {}%",
            r.savings_percentage,
        );
        // Reference + delta totals add up to on-disk cost.
        assert_eq!(r.total_stored_bytes, 200_000 + 2 * 1_000);
        assert_eq!(r.total_original_bytes, 2 * 200_000);
    }

    #[test]
    fn empty_bucket_returns_zero_savings_without_division_by_zero() {
        let acc = StatsAcc::default();
        let r = acc.into_result("empty");
        assert_eq!(r.total_objects, 0);
        assert_eq!(r.total_original_bytes, 0);
        assert_eq!(r.total_stored_bytes, 0);
        assert_eq!(r.savings_percentage, 0.0);
    }

    #[test]
    fn cache_key_pattern_matches_python() {
        assert_eq!(cache_key_for(Mode::Quick), ".deltaglider/stats_quick.json");
        assert_eq!(
            cache_key_for(Mode::Sampled),
            ".deltaglider/stats_sampled.json"
        );
        assert_eq!(
            cache_key_for(Mode::Detailed),
            ".deltaglider/stats_detailed.json"
        );
    }

    #[test]
    fn cache_validity_compares_count_and_size() {
        let a = Validation {
            object_count: 100,
            compressed_size: 5_000_000,
        };
        let same = Validation {
            object_count: 100,
            compressed_size: 5_000_000,
        };
        let count_drifted = Validation {
            object_count: 101,
            compressed_size: 5_000_000,
        };
        let size_drifted = Validation {
            object_count: 100,
            compressed_size: 5_000_001,
        };
        assert!(is_cache_valid(&a, &same));
        assert!(!is_cache_valid(&a, &count_drifted));
        assert!(!is_cache_valid(&a, &size_drifted));
    }

    #[test]
    fn internal_keys_are_filtered_from_stats() {
        assert!(is_internal_key(".deltaglider/stats_quick.json"));
        assert!(is_internal_key(".deltaglider/config.db"));
        assert!(!is_internal_key("releases/v1.zip"));
        assert!(!is_internal_key("deltaglider-not-internal.txt"));
    }

    #[test]
    fn mode_default_is_quick_when_no_flag_given() {
        let args = StatsArgs {
            url: "s3://b".into(),
            quick: false,
            sampled: false,
            detailed: false,
            refresh: false,
            no_cache: false,
            json: false,
            endpoint_url: None,
            region: None,
            profile: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
        };
        assert_eq!(args.mode(), Mode::Quick);
    }

    #[test]
    fn mode_picks_detailed_when_flag_set() {
        let mut args = StatsArgs {
            url: "s3://b".into(),
            quick: false,
            sampled: false,
            detailed: true,
            refresh: false,
            no_cache: false,
            json: false,
            endpoint_url: None,
            region: None,
            profile: None,
            access_key_id: None,
            secret_access_key: None,
            force_path_style: false,
        };
        assert_eq!(args.mode(), Mode::Detailed);
        args.detailed = false;
        args.sampled = true;
        assert_eq!(args.mode(), Mode::Sampled);
    }

    #[test]
    fn cache_doc_round_trips_through_json() {
        let doc = CacheDoc {
            version: CACHE_VERSION.into(),
            mode: Mode::Sampled,
            computed_at: Utc::now(),
            validation: Validation {
                object_count: 42,
                compressed_size: 1_234_567,
            },
            stats: StatsResult {
                bucket: "demo".into(),
                total_objects: 42,
                total_original_bytes: 100_000_000,
                total_stored_bytes: 1_234_567,
                savings_percentage: 98.77,
                deltaspace_health: DeltaspaceHealth {
                    excellent: 5,
                    good: 2,
                    fair: 1,
                    poor: 0,
                    no_reference: 1,
                },
            },
        };
        let body = serde_json::to_vec(&doc).unwrap();
        let parsed: CacheDoc = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.version, CACHE_VERSION);
        assert_eq!(parsed.mode, Mode::Sampled);
        assert_eq!(parsed.validation.object_count, 42);
        assert_eq!(parsed.stats.deltaspace_health.excellent, 5);
    }
}
