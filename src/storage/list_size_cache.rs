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
    /// The client-visible ETag, without quotes (`md5` or `md5-N`; empty
    /// for a delta without `dg-md5`, whose HEAD sends `""`).
    pub etag: String,
}

impl LogicalFacts {
    /// The logical facts a full metadata record carries. `None` for a
    /// sealed (format v2) object: its clear metadata holds no logical
    /// facts, only the encrypting wrapper can open them. THE one place that
    /// derives listing facts from metadata (source guard
    /// `logical_facts_of_is_the_only_constructor_from_metadata`).
    pub fn of(meta: &FileMetadata) -> Option<Self> {
        if is_sealed(meta) {
            return None;
        }
        Some(Self {
            size: meta.file_size,
            etag: bare_etag(&meta.etag()).to_string(),
        })
    }
}

/// Does this metadata carry the format-v2 seal
/// ([`crate::types::meta_keys::SEALED`])?
pub fn is_sealed(meta: &FileMetadata) -> bool {
    meta.user_metadata
        .contains_key(crate::types::meta_keys::SEALED)
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
/// facts already are the logical ones (a plain passthrough object) or when
/// the stored ETag is unknown. The logical ETag may be empty: a delta from
/// an older toolchain carries no `dg-md5`, and its HEAD and GET send the
/// empty ETag (`""`), so its LIST entry does too.
pub fn record(id: &StoredObjectId<'_>, facts: LogicalFacts) {
    if bare_etag(id.etag).is_empty() || !differs(id, &facts) {
        return;
    }
    CACHE.insert(cache_key(id), facts);
}

/// [`record`], also when the stored facts are the logical ones: for an
/// object that a LIST asked about (see `listing_facts::asked_facts_key`),
/// so the next LIST of this process finds it without a facts read.
pub fn record_asked(id: &StoredObjectId<'_>, facts: LogicalFacts) {
    if bare_etag(id.etag).is_empty() {
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
/// [`crate::storage::listing_facts`]), and when. The next HEAD that learns
/// their facts writes them (lazy backfill for objects stored before the
/// facts existed), and for [`MISSING_FACTS_TTL`] a LIST does not read the
/// facts of the object again. Bounded: an entry that falls out only waits
/// for a later LIST to mark it.
static MISSING: LazyLock<Cache<String, std::time::Instant>> =
    LazyLock::new(|| Cache::builder().max_capacity(100_000).build());

/// How long a LIST trusts "no durable facts" for one stored object. Facts
/// that another node writes in that time are seen at most this late; an
/// overwrite is another stored object (new ETag), so it is read at once.
pub const MISSING_FACTS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Note that a LIST found no durable facts for this stored object.
pub fn mark_missing_facts(id: &StoredObjectId<'_>) {
    if !bare_etag(id.etag).is_empty() {
        MISSING.insert(cache_key(id), std::time::Instant::now());
    }
}

/// Did a LIST find no durable facts for this stored object less than
/// [`MISSING_FACTS_TTL`] ago? Then a LIST skips the facts read for it: the
/// answer would be the same.
pub fn missing_recently(id: &StoredObjectId<'_>) -> bool {
    MISSING
        .get(&cache_key(id))
        .is_some_and(|at| at.elapsed() < MISSING_FACTS_TTL)
}

/// Is this stored object still marked by [`mark_missing_facts`], whatever
/// the mark's age? `false` once a HEAD learned its facts (and took the
/// mark): the facts healer then sends no HEAD of its own.
pub fn still_missing(id: &StoredObjectId<'_>) -> bool {
    MISSING.contains_key(&cache_key(id))
}

/// Was this stored object marked by [`mark_missing_facts`]? Clears the mark,
/// so one object is backfilled once.
pub fn take_missing_facts(id: &StoredObjectId<'_>) -> bool {
    MISSING.remove(&cache_key(id)).is_some()
}

/// Stored objects whose HEAD showed sealed metadata: their logical facts
/// cannot be learned from a HEAD (only the encrypting wrapper opens them,
/// and it writes `!!2` listing tokens instead). A LIST then neither reads
/// their `!!1` facts nor queues a healer HEAD for them. Exact per stored
/// object (an overwrite is another entry), so no TTL; bounded.
static SEALED: LazyLock<Cache<String, ()>> =
    LazyLock::new(|| Cache::builder().max_capacity(100_000).build());

/// Note that the stored object `id` is sealed. Takes its
/// [`mark_missing_facts`] mark: the healer then sends no HEAD for it.
pub fn mark_sealed(id: &StoredObjectId<'_>) {
    if bare_etag(id.etag).is_empty() {
        return;
    }
    let key = cache_key(id);
    MISSING.remove(&key);
    SEALED.insert(key, ());
}

/// Did a HEAD show that the stored object `id` is sealed?
pub fn known_sealed(id: &StoredObjectId<'_>) -> bool {
    SEALED.contains_key(&cache_key(id))
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

/// How [`crate::storage::StorageBackend::resolve_listed_sizes`] treats one
/// listing. `Default` trusts the `!!1` facts (`trust_v1_facts: true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolveListed {
    /// A passthrough entry may be stored in another form (the
    /// proxy-encryption wrapper sets it), so its facts are looked up too.
    pub passthrough_may_differ: bool,
    /// The caller HEADs the entries whose facts stay unknown (a
    /// `metadata=true` LIST, the lifecycle scan, a savings scan that HEADs
    /// its unknowns). That HEAD learns the facts and writes them, so the
    /// backend queues no background HEAD of its own: two HEADs per object
    /// otherwise.
    pub caller_heads_misses: bool,
    /// Use the `!!1` listing facts and the in-process entries learned from
    /// them. `false` in a bucket whose objects are sealed (format v2): any
    /// `!!1` entry there is unauthenticated, and the wrapper resolves the
    /// sizes from its `!!2` listing tokens instead.
    pub trust_v1_facts: bool,
}

impl Default for ResolveListed {
    fn default() -> Self {
        Self {
            passthrough_may_differ: false,
            caller_heads_misses: false,
            trust_v1_facts: true,
        }
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
    /// As `Cached` for a delta whose stored object is a proxy ciphertext:
    /// the listed `.delta` size is the ciphertext size, while the delta size
    /// that HeadObject shows (`dg-delta-size`) is the plaintext one, which
    /// only a HEAD reads (a `metadata=true` LIST sends one).
    CachedEncryptedDelta,
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

    /// A facts miss is remembered for exactly the stored object it was
    /// found for: an overwrite (new ETag or size) reads the facts again.
    #[test]
    fn a_facts_miss_is_remembered_per_stored_object() {
        let key = "neg/fw.tar.delta";
        assert!(!missing_recently(&id(key, "e1", 46)));
        mark_missing_facts(&id(key, "\"e1\"", 46));
        assert!(missing_recently(&id(key, "e1", 46)));
        assert!(!missing_recently(&id(key, "e2", 46)), "overwritten");
        assert!(!missing_recently(&id(key, "e1", 47)), "other size");
        // An unknown stored ETag is never marked.
        mark_missing_facts(&id("neg/x.delta", "", 1));
        assert!(!missing_recently(&id("neg/x.delta", "", 1)));
        // The HEAD that learns the facts takes the mark.
        assert!(still_missing(&id(key, "e1", 46)));
        assert!(take_missing_facts(&id(key, "e1", 46)));
        assert!(!missing_recently(&id(key, "e1", 46)));
        assert!(!still_missing(&id(key, "e1", 46)));
    }

    /// A sealed object has no facts in clear: `of` refuses it, so no PUT,
    /// copy, HEAD backfill or healer path records or writes `!!1` facts.
    #[test]
    fn sealed_metadata_has_no_logical_facts() {
        let mut m = FileMetadata::new_passthrough(
            "a.bin".into(),
            "s".repeat(64),
            "0123456789abcdef0123456789abcdef".into(),
            10,
            None,
        );
        assert_eq!(
            LogicalFacts::of(&m),
            Some(LogicalFacts {
                size: 10,
                etag: "0123456789abcdef0123456789abcdef".into(),
            })
        );
        m.user_metadata
            .insert(crate::types::meta_keys::SEALED.into(), "2.x".into());
        assert_eq!(LogicalFacts::of(&m), None);
    }

    /// A sealed stored object is remembered exactly, and the mark takes
    /// its "missing facts" mark, so the healer sends no HEAD.
    #[test]
    fn a_sealed_mark_replaces_the_missing_mark() {
        let key = "sealed/fw.tar.delta";
        mark_missing_facts(&id(key, "e1", 46));
        assert!(!known_sealed(&id(key, "e1", 46)));
        mark_sealed(&id(key, "\"e1\"", 46));
        assert!(known_sealed(&id(key, "e1", 46)));
        assert!(!still_missing(&id(key, "e1", 46)));
        assert!(!known_sealed(&id(key, "e2", 46)), "overwritten");
        mark_sealed(&id("sealed/x.delta", "", 1));
        assert!(!known_sealed(&id("sealed/x.delta", "", 1)), "no ETag");
    }

    #[test]
    fn resolve_listed_trusts_v1_facts_by_default() {
        assert!(ResolveListed::default().trust_v1_facts);
    }

    /// `LogicalFacts::of` is the only place that derives listing facts from
    /// metadata, so its seal check covers every facts write. The other
    /// literal builds them from a parsed `!!1` key.
    #[test]
    fn logical_facts_of_is_the_only_constructor_from_metadata() {
        use crate::source_scan::{prod_sources, prod_text, read};
        let literal = |text: &str| {
            prod_text(text)
                .replace("struct LogicalFacts {", "")
                .replace("impl LogicalFacts {", "")
                .matches("LogicalFacts {")
                .count()
        };
        let builders: Vec<(String, usize)> = prod_sources("src")
            .into_iter()
            .map(|(file, text)| {
                let n = literal(&text);
                (file, n)
            })
            .filter(|(_, n)| *n > 0)
            .collect();
        assert_eq!(
            builders,
            [("src/storage/listing_facts.rs".to_string(), 1)],
            "build listing facts from metadata only through LogicalFacts::of"
        );
        assert!(
            !prod_text(&read("src/storage/listing_facts.rs")).contains("FileMetadata"),
            "listing_facts.rs builds facts from keys, never from metadata"
        );
        let own = prod_text(&read("src/storage/list_size_cache.rs"));
        let of = &own[own.find("pub fn of(meta: &FileMetadata)").unwrap()..];
        let of = &of[..of.find("\n    }\n").unwrap()];
        assert!(of.contains("if is_sealed(meta) {"), "{of}");
        let self_literals = own.matches("Self {").count() - own.matches("-> Self {").count();
        assert_eq!(self_literals, 2, "`of` and `ResolveListed::default`");
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
    fn plain_objects_and_unknown_stored_etags_are_not_recorded() {
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
        // A delta without `dg-md5`: its HEAD sends the empty ETag, and the
        // entry keeps it, so the LIST shows what the HEAD shows.
        let no_md5 = LogicalFacts {
            size: 99,
            etag: String::new(),
        };
        record(&id("t2/legacy.delta", "s", 5), no_md5.clone());
        assert_eq!(lookup(&id("t2/legacy.delta", "s", 5)), Some(no_md5));
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
