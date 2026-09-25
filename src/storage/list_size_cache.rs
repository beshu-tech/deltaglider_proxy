// SPDX-License-Identifier: BUSL-1.1

//! Listing-size cache: the logical size and ETag of a STORED object, keyed by
//! that stored object's identity as a LIST reports it.
//!
//! A LIST on an S3 backend returns the stored object: for a delta, the
//! `.delta` object (its size, its ETag); on a proxy-encrypted backend, the
//! ciphertext. The size and ETag a client must see are the logical ones, and
//! those live only in the object's user metadata, which a LIST does not
//! return. Reading them costs one HEAD per object, and a client LIST must not
//! send those (issue #82: a LIST of a large prefix was already slow).
//!
//! This cache remembers the logical facts every time the backend learns them
//! anyway (a PUT it sends, a HEAD it sends), keyed by
//! `(scope, bucket, stored key, stored ETag, stored size)`. A LIST then looks
//! each entry up by the facts the LIST itself carries. The key changes when
//! the stored object changes (a new PUT has a new ETag), so an entry can never
//! describe a different object than the listed one: no TTL, no invalidation,
//! and no staleness across proxy instances. A miss costs nothing: the entry
//! keeps its stored size, as before.
//!
//! Process-global on purpose: an engine rebuild (config reload) builds new
//! backend instances, and the entries stay valid because the key names the
//! stored object, not the backend instance.

use moka::sync::Cache;
use std::sync::LazyLock;

use crate::types::FileMetadata;

/// The stored object as a LIST entry describes it. `etag` is compared without
/// quotes.
#[derive(Debug, Clone, Copy)]
pub struct StoredObjectId<'a> {
    /// Which storage endpoint holds the bucket (an S3 endpoint URL).
    pub scope: &'a str,
    pub bucket: &'a str,
    /// The stored key (`dir/file.tar.delta` for a delta).
    pub key: &'a str,
    pub etag: &'a str,
    pub size: u64,
}

/// What a client must see for that stored object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalFacts {
    pub size: u64,
    /// The client-visible ETag, without quotes (`md5` or `md5-N`).
    pub etag: String,
}

impl LogicalFacts {
    /// The logical facts a full metadata record carries.
    pub fn of(meta: &FileMetadata) -> Self {
        Self {
            size: meta.file_size,
            etag: bare_etag(&meta.etag()).to_string(),
        }
    }
}

fn bare_etag(etag: &str) -> &str {
    etag.trim_matches('"')
}

fn cache_key(id: &StoredObjectId<'_>) -> String {
    // NUL cannot occur in an endpoint, a bucket name, an S3 key or an ETag.
    format!(
        "{}\0{}\0{}\0{}\0{}",
        id.scope,
        id.bucket,
        id.key,
        bare_etag(id.etag),
        id.size
    )
}

/// Default byte budget. An entry weighs its cache key (endpoint, bucket,
/// stored key, stored ETag, size: about 145 bytes with a 60-byte object key)
/// plus the original ETag (32) plus 100 bytes of overhead, so about 275
/// bytes, and 32 MiB holds about 120,000 objects.
const DEFAULT_BUDGET_MB: u64 = 32;

static CACHE: LazyLock<Cache<String, LogicalFacts>> = LazyLock::new(|| {
    let mb = crate::config::env_parse_with_default("DGP_LIST_SIZE_CACHE_MB", DEFAULT_BUDGET_MB);
    Cache::builder()
        .max_capacity(mb.saturating_mul(1024 * 1024))
        .weigher(|k: &String, v: &LogicalFacts| -> u32 {
            (k.len() + v.etag.len() + 100).min(u32::MAX as usize) as u32
        })
        .build()
});

/// Does a stored object need an entry? Only when a LIST of it would report
/// something else than the client must see.
pub fn differs(id: &StoredObjectId<'_>, facts: &LogicalFacts) -> bool {
    id.size != facts.size || bare_etag(id.etag) != facts.etag
}

/// Remember the logical facts of a stored object. A no-op when the stored
/// facts already are the logical ones (a plain passthrough object), when the
/// stored ETag is unknown, or when the logical ETag is unknown (a delta from
/// a legacy toolchain without `dg-md5`): a LIST must never report an empty
/// ETag.
pub fn record(id: &StoredObjectId<'_>, facts: LogicalFacts) {
    if bare_etag(id.etag).is_empty() || facts.etag.is_empty() || !differs(id, &facts) {
        return;
    }
    CACHE.insert(cache_key(id), facts);
}

/// The logical facts of exactly this stored object, if the process learned
/// them.
pub fn lookup(id: &StoredObjectId<'_>) -> Option<LogicalFacts> {
    if bare_etag(id.etag).is_empty() {
        return None;
    }
    CACHE.get(&cache_key(id))
}

/// Stored objects that a LIST found without durable listing facts (see
/// [`crate::storage::listing_facts`]). The next HEAD that learns their facts
/// writes them (lazy backfill for objects stored before the facts existed).
/// Bounded: an entry that falls out only waits for a later LIST to mark it.
static MISSING: LazyLock<Cache<String, ()>> =
    LazyLock::new(|| Cache::builder().max_capacity(100_000).build());

/// Note that a LIST found no durable facts for this stored object.
pub fn mark_missing_facts(id: &StoredObjectId<'_>) {
    if !bare_etag(id.etag).is_empty() {
        MISSING.insert(cache_key(id), ());
    }
}

/// Was this stored object marked by [`mark_missing_facts`]? Clears the mark,
/// so one object is backfilled once.
pub fn take_missing_facts(id: &StoredObjectId<'_>) -> bool {
    MISSING.remove(&cache_key(id)).is_some()
}

/// Replace the size and ETag of a listed entry with the logical ones. Only
/// those two fields change: `created_at` stays the listed LastModified, so
/// consecutive LISTs of an unchanged object report the same timestamp whether
/// the cache holds the object or not.
pub fn apply(meta: &mut FileMetadata, facts: &LogicalFacts) {
    meta.file_size = facts.size;
    if facts.etag.contains('-') {
        meta.multipart_etag = Some(format!("\"{}\"", facts.etag));
    } else {
        meta.md5 = facts.etag.clone();
        meta.multipart_etag = None;
    }
}

/// How much of a listed entry's size a LIST knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListedSize {
    /// The listing itself carries the logical size (a plain passthrough
    /// object, a directory marker, or a backend that lists logical sizes).
    Listed,
    /// The listing-size cache or the durable listing facts supplied the
    /// logical size and ETag.
    Cached,
    /// Only the stored size is known.
    StoredOnly,
}

impl ListedSize {
    pub fn is_known(self) -> bool {
        !matches!(self, ListedSize::StoredOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StorageInfo;

    fn id<'a>(key: &'a str, etag: &'a str, size: u64) -> StoredObjectId<'a> {
        StoredObjectId {
            scope: "http://unit-test-scope",
            bucket: "b",
            key,
            etag,
            size,
        }
    }

    #[test]
    fn hit_only_for_the_same_stored_object() {
        let facts = LogicalFacts {
            size: 3_000_000,
            etag: "orig".into(),
        };
        record(&id("t1/fw.tar.delta", "\"d1\"", 46), facts.clone());
        // Quotes do not matter.
        assert_eq!(lookup(&id("t1/fw.tar.delta", "d1", 46)), Some(facts));
        // Another node overwrote the object: new ETag, even with the same
        // size, is a different stored object.
        assert_eq!(lookup(&id("t1/fw.tar.delta", "d2", 46)), None);
        assert_eq!(lookup(&id("t1/fw.tar.delta", "d1", 47)), None);
        assert_eq!(lookup(&id("t1/other.tar.delta", "d1", 46)), None);
    }

    #[test]
    fn plain_objects_and_unknown_etags_are_not_recorded() {
        let same = LogicalFacts {
            size: 10,
            etag: "e".into(),
        };
        record(&id("t2/plain.txt", "e", 10), same);
        assert_eq!(lookup(&id("t2/plain.txt", "e", 10)), None);
        let facts = LogicalFacts {
            size: 99,
            etag: "x".into(),
        };
        record(&id("t2/noetag.delta", "", 5), facts);
        assert_eq!(lookup(&id("t2/noetag.delta", "", 5)), None);
        // Legacy delta without `dg-md5`: no logical ETag, no entry.
        let no_md5 = LogicalFacts {
            size: 99,
            etag: String::new(),
        };
        record(&id("t2/legacy.delta", "s", 5), no_md5);
        assert_eq!(lookup(&id("t2/legacy.delta", "s", 5)), None);
    }

    #[test]
    fn apply_replaces_only_size_and_etag() {
        let listed_at = chrono::Utc::now() - chrono::Duration::days(3);
        let mut m = FileMetadata::fallback(
            "fw.tar".into(),
            46,
            "d1".into(),
            listed_at,
            None,
            StorageInfo::delta_stub(46),
        );
        apply(
            &mut m,
            &LogicalFacts {
                size: 3_000_000,
                etag: "orig".into(),
            },
        );
        assert_eq!(m.file_size, 3_000_000);
        assert_eq!(m.etag(), "\"orig\"");
        assert_eq!(m.created_at, listed_at, "LastModified stays the listed one");
        assert_eq!(m.delta_size(), Some(46), "stored size stays known");
        apply(
            &mut m,
            &LogicalFacts {
                size: 5,
                etag: "abc-3".into(),
            },
        );
        assert_eq!(m.etag(), "\"abc-3\"");
    }
}
