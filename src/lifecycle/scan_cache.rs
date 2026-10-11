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

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moka::sync::Cache;

use crate::deltaglider::{DynEngine, EngineError, ListObjectsPage};
use crate::types::FileMetadata;

/// The memory the cache may use, about 80,000 objects.
const MAX_BYTES: u64 = 32 << 20;

/// An entry not listed for this long goes (the rule was removed or
/// narrowed). Longer than the default hourly tick.
const IDLE: Duration = Duration::from_secs(3 * 3600);

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
}

/// The scan cache of one engine.
pub(crate) struct ScanCache {
    entries: Cache<String, Arc<(StoredId, FileMetadata)>>,
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
        }
    }

    /// The cache of `engine`.
    pub(crate) fn of(engine: &Arc<DynEngine>) -> Arc<Self> {
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
    /// metadata, as `list_objects(.., metadata = true)` returns it. An
    /// object whose stored identity is unchanged since a listing of this
    /// cache costs no request beyond the page's LIST; the others are
    /// completed as that listing completes them (the engine's metadata
    /// cache, then a HEAD) and kept.
    pub(crate) async fn list_page(
        &self,
        engine: &DynEngine,
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
        if !misses.is_empty() {
            let mut rest = ListObjectsPage {
                objects: misses,
                common_prefixes: Vec::new(),
                is_truncated: false,
                next_continuation_token: None,
                facts_missing_keys: Vec::new(),
            };
            engine.finish_listed_page(bucket, &mut rest, true).await?;
            for (key, meta) in rest.objects {
                // A stub that a throttled HEAD sweep left holds the stored
                // size: never kept.
                if let Some(id) = ids
                    .remove(&key)
                    .filter(|_| !meta.is_unresolved_delta_stub())
                {
                    self.entries
                        .insert(Self::key(bucket, &key), Arc::new((id, meta.clone())));
                }
                objects.push((key, meta));
            }
            page.facts_missing_keys = rest.facts_missing_keys;
        }
        objects.sort_by(|a, b| a.0.cmp(&b.0));
        page.objects = objects;
        Ok(page)
    }
}
