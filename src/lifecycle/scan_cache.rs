// SPDX-License-Identifier: BUSL-1.1

//! The logical metadata of the objects that lifecycle rules list, kept
//! from one run to the next for as long as the stored object is the same.
//!
//! A lifecycle plan needs each object's `created_at` and logical size. On
//! S3 a LIST reports the STORED object (a delta's `.delta`, with the
//! backend's LastModified), so the listing sent a HEAD for every delta or
//! delta-eligible key. The engine's metadata cache keeps those answers
//! for 10 minutes, and the hourly rescan sent every HEAD again: about
//! 100,000 HEAD requests an hour for a bucket where nothing expires.
//!
//! This cache keeps the answer under the stored identity that the lite
//! listing shows: key, ETag, size and LastModified. An overwrite changes
//! LastModified (and the ETag), so it misses and is read again. A preview
//! and a run of the same engine share the cache, so a run right after a
//! preview sends no HEAD for an unchanged object.
//!
//! The cache is also kept in the node-local jobs DB (`scan_store`), so the
//! first scan after a restart or a config apply (a new engine, an empty
//! in-memory cache) sends no HEAD for an unchanged object either. Without
//! a jobs DB (a key mismatch) the scan uses the in-memory cache only.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moka::sync::Cache;
use tokio::sync::Mutex as TokioMutex;
use tracing::debug;

use super::scan_store::ScanRow;
use crate::config_db::JobsDb;
use crate::deltaglider::{DynEngine, EngineError, ListObjectsPage};
use crate::types::FileMetadata;

/// The memory the cache may use, about 80,000 objects.
const MAX_BYTES: u64 = 32 << 20;

/// An entry not listed for this long goes (the rule was removed or
/// narrowed). Longer than the default hourly tick.
const IDLE: Duration = Duration::from_secs(3 * 3600);

/// A persisted row that no scan saw for this long goes (a rule removed or
/// narrowed, an object deleted): a week covers a daily rule and a deploy.
const DB_IDLE_SECS: i64 = 7 * 24 * 3600;
/// Rows the jobs DB keeps at most (about 100 MB at 500 bytes a row); past
/// it the least recently seen go.
const DB_MAX_ROWS: u64 = 200_000;
/// A row a scan finds is marked seen again only when its mark is older than
/// this: a scan of unchanged objects then writes nothing most hours.
const DB_TOUCH_AFTER_SECS: i64 = 24 * 3600;
/// The prune runs at most this often per engine.
const DB_PRUNE_EVERY_SECS: i64 = 600;

/// A lifecycle scan's view of the cache: the engine's in-memory cache and
/// the jobs DB that persists it (`None`: memory only).
pub(crate) struct Scan {
    cache: Arc<ScanCache>,
    db: Option<Arc<TokioMutex<JobsDb>>>,
}

impl Scan {
    /// See [`ScanCache::list_page`].
    pub(crate) async fn list_page(
        &self,
        engine: &DynEngine,
        bucket: &str,
        prefix: &str,
        max_keys: u32,
        token: Option<&str>,
    ) -> Result<ListObjectsPage, EngineError> {
        self.cache
            .list_page(engine, self.db.as_ref(), bucket, prefix, max_keys, token)
            .await
    }
}

/// What a lite LIST shows of a stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredId {
    etag: String,
    size: u64,
    last_modified: DateTime<Utc>,
}

impl StoredId {
    /// From a lite listing entry: there `created_at` is the backend's
    /// LastModified and the size and ETag are the stored ones.
    fn of(lite: &FileMetadata) -> Self {
        Self {
            etag: lite.etag(),
            size: lite.file_size,
            last_modified: lite.created_at,
        }
    }

    /// Does the persisted row describe this stored object? LastModified
    /// in milliseconds, as the row keeps it.
    fn matches(&self, row: &ScanRow) -> bool {
        self.etag == row.etag
            && self.size == row.size
            && self.last_modified.timestamp_millis() == row.last_modified_ms
    }

    fn row(&self, key: &str, meta: &FileMetadata) -> Option<ScanRow> {
        Some(ScanRow {
            key: key.to_string(),
            etag: self.etag.clone(),
            size: self.size,
            last_modified_ms: self.last_modified.timestamp_millis(),
            meta_json: serde_json::to_string(meta).ok()?,
        })
    }
}

/// The scan cache of one engine.
pub(crate) struct ScanCache {
    entries: Cache<String, Arc<(StoredId, FileMetadata)>>,
    /// Unix seconds of this engine's last prune of the persisted rows.
    last_prune: AtomicI64,
}

/// One cache per live engine: a config apply builds a new engine (it can
/// route a bucket to another backend), which starts with an empty cache.
static CACHES: Mutex<Vec<(Weak<DynEngine>, Arc<ScanCache>)>> = Mutex::new(Vec::new());

impl ScanCache {
    fn new() -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(MAX_BYTES)
                .weigher(
                    |key: &String, value: &Arc<(StoredId, FileMetadata)>| -> u32 {
                        let meta: usize = value
                            .1
                            .user_metadata
                            .iter()
                            .map(|(k, v)| k.len() + v.len() + 80)
                            .sum();
                        (key.len() + value.0.etag.len() + 450 + meta) as u32
                    },
                )
                .time_to_idle(IDLE)
                .build(),
            last_prune: AtomicI64::new(0),
        }
    }

    /// The scan of `engine`: its in-memory cache, persisted in `db` (the
    /// node-local jobs DB) when there is one.
    pub(crate) fn of(engine: &Arc<DynEngine>, db: Option<Arc<TokioMutex<JobsDb>>>) -> Scan {
        Scan {
            cache: Self::memory_of(engine),
            db,
        }
    }

    /// The in-memory cache of `engine`.
    fn memory_of(engine: &Arc<DynEngine>) -> Arc<Self> {
        let mut caches = CACHES.lock().unwrap_or_else(|e| e.into_inner());
        caches.retain(|(engine, _)| engine.strong_count() > 0);
        if let Some((_, cache)) = caches
            .iter()
            .find(|(e, _)| std::ptr::eq(e.as_ptr(), Arc::as_ptr(engine)))
        {
            return cache.clone();
        }
        let cache = Arc::new(Self::new());
        caches.push((Arc::downgrade(engine), cache.clone()));
        cache
    }

    fn key(bucket: &str, key: &str) -> String {
        format!("{bucket}/{key}")
    }

    /// One page of `bucket` under `prefix`, with each object's logical
    /// metadata (`finish_listed_page_with_heads`). An object whose stored
    /// identity is unchanged since a listing of this cache, or since one
    /// that `db` persisted, costs no request beyond the page's LIST; the
    /// others are completed as that listing completes them (the engine's
    /// metadata cache, then a HEAD) and kept. The page's rows go to `db` in
    /// one transaction.
    async fn list_page(
        &self,
        engine: &DynEngine,
        db: Option<&Arc<TokioMutex<JobsDb>>>,
        bucket: &str,
        prefix: &str,
        max_keys: u32,
        token: Option<&str>,
    ) -> Result<ListObjectsPage, EngineError> {
        let mut page = engine
            .list_objects_lite(bucket, prefix, None, max_keys, token)
            .await?;
        let mut objects = Vec::with_capacity(page.objects.len());
        let mut misses = Vec::new();
        let mut ids = std::collections::HashMap::new();
        for (key, lite) in std::mem::take(&mut page.objects) {
            let id = StoredId::of(&lite);
            match self.entries.get(&Self::key(bucket, &key)) {
                Some(hit) if hit.0 == id => objects.push((key, hit.1.clone())),
                _ => {
                    ids.insert(key.clone(), id);
                    misses.push((key, lite));
                }
            }
        }
        let now = Utc::now().timestamp();
        let mut seen = Vec::new();
        if let (Some(db), false) = (db, misses.is_empty()) {
            let keys: Vec<&str> = misses.iter().map(|(k, _)| k.as_str()).collect();
            let rows = match db.lock().await.lifecycle_scan_get(bucket, &keys) {
                Ok(rows) => rows,
                Err(e) => {
                    debug!("lifecycle scan cache of {bucket} not read: {e}");
                    Vec::new()
                }
            };
            let persisted: std::collections::HashMap<String, (ScanRow, i64)> = rows
                .into_iter()
                .map(|(r, at)| (r.key.clone(), (r, at)))
                .collect();
            misses.retain(|(key, _)| {
                let Some((row, seen_at)) = persisted.get(key) else {
                    return true;
                };
                let id = &ids[key];
                let meta = id
                    .matches(row)
                    .then(|| serde_json::from_str::<FileMetadata>(&row.meta_json).ok())
                    .flatten();
                let Some(meta) = meta else { return true };
                if now - seen_at >= DB_TOUCH_AFTER_SECS {
                    seen.push(key.clone());
                }
                self.entries.insert(
                    Self::key(bucket, key),
                    Arc::new((ids[key].clone(), meta.clone())),
                );
                objects.push((key.clone(), meta));
                false
            });
        }
        let mut upserts = Vec::new();
        if !misses.is_empty() {
            let mut rest = ListObjectsPage {
                objects: misses,
                common_prefixes: Vec::new(),
                is_truncated: false,
                next_continuation_token: None,
                facts_missing_keys: Vec::new(),
            };
            engine
                .finish_listed_page_with_heads(bucket, &mut rest)
                .await?;
            for (key, meta) in rest.objects {
                // A stub that a throttled HEAD sweep left holds the stored
                // size: never kept.
                if let Some(id) = ids
                    .remove(&key)
                    .filter(|_| !meta.is_unresolved_delta_stub())
                {
                    if db.is_some() {
                        upserts.extend(id.row(&key, &meta));
                    }
                    self.entries
                        .insert(Self::key(bucket, &key), Arc::new((id, meta.clone())));
                }
                objects.push((key, meta));
            }
            page.facts_missing_keys = rest.facts_missing_keys;
        }
        if let Some(db) = db {
            self.persist(db, bucket, &upserts, &seen, now).await;
        }
        objects.sort_by(|a, b| a.0.cmp(&b.0));
        page.objects = objects;
        Ok(page)
    }

    /// Write one page's rows (one transaction), and prune now and then.
    /// Best effort: a failed write costs a HEAD after the next restart.
    async fn persist(
        &self,
        db: &Arc<TokioMutex<JobsDb>>,
        bucket: &str,
        upserts: &[ScanRow],
        seen: &[String],
        now: i64,
    ) {
        let prune = now - self.last_prune.load(Ordering::Relaxed) >= DB_PRUNE_EVERY_SECS;
        if upserts.is_empty() && seen.is_empty() && !prune {
            return;
        }
        let db = db.lock().await;
        let seen: Vec<&str> = seen.iter().map(String::as_str).collect();
        if let Err(e) = db.lifecycle_scan_record(bucket, upserts, &seen, now) {
            debug!("lifecycle scan cache of {bucket} not written: {e}");
        }
        if prune {
            self.last_prune.store(now, Ordering::Relaxed);
            if let Err(e) = db.lifecycle_scan_prune(now, DB_IDLE_SECS, DB_MAX_ROWS) {
                debug!("lifecycle scan cache not pruned: {e}");
            }
        }
    }
}
