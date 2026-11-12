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
    /// The logical facts of an opened listing token: authenticated by the
    /// key that sealed them, never derived from clear metadata. `etag` is
    /// bare (`md5` or `md5-N`).
    pub fn of_listing_token(size: u64, etag: String) -> Self {
        Self { size, etag }
    }

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

/// An ETag without its quotes. A PUT response quotes the ETag, a listing
/// does not: THE bare form, which every cache key, marker and listing token
/// binds, so both sides agree.
pub(crate) fn bare_etag(etag: &str) -> &str {
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

/// Facts writes that the full write queue refused (L5, #171), by stored
/// object: the facts key to write. This process lists such an object right
/// (its [`record`] entry), so no LIST of it finds a miss and the healer
/// never runs for it: the durable facts would stay unwritten, and every
/// other node and every restart would list the stored size. A LIST of the
/// object takes the entry and offers the write again
/// ([`take_unwritten`]).
///
/// The registry is a moka cache (capacity [`UNWRITTEN_CAP`], entries live
/// [`UNWRITTEN_TTL`]), so its eviction costs O(1) amortised and takes no
/// global lock on the request path. It is keyed by the stored OBJECT (scope,
/// bucket, key), not by its ETag: the entry of an object that was deleted
/// ([`forget_unwritten`], from the delete path) or written again with a write
/// that was queued ([`forget_unwritten`], from the PUT path) is removed at
/// once, and one for another version than the listed one is dropped when the
/// LIST meets it. An entry that the cache evicts for room is counted
/// (`deltaglider_listing_facts_queue_dropped_total{queue="unwritten"}`) and
/// logged (the first, then every 10,000th).
static UNWRITTEN: LazyLock<Registry> =
    LazyLock::new(|| Registry::new(UNWRITTEN_CAP, UNWRITTEN_TTL));

#[derive(Clone)]
struct Unwritten {
    etag: String,
    size: u64,
    facts_key: String,
}

/// Most entries of [`UNWRITTEN`].
const UNWRITTEN_CAP: u64 = 100_000;
/// How long an entry of [`UNWRITTEN`] lives.
const UNWRITTEN_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
/// The registry logs its first drop and every this many after it.
const UNWRITTEN_LOG_EVERY: u64 = 10_000;

/// The refused-writes cache with an exact count of its entries.
///
/// The count (`live`) answers [`any_unwritten`] with one atomic load, so a LIST
/// page pays nothing when no write was refused, and no housekeeping call
/// runs on the request path. It is exact because each entry leaves it once:
/// a fresh insert counts one, an explicit removal (`take`, `forget`) takes
/// one, and the eviction listener takes one for expiry and for room (an
/// explicit removal and a replacement are the caller's, not the listener's).
struct Registry {
    cache: Cache<String, Unwritten>,
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Entries dropped for room, for the log's rate limit (the listener owns
    /// a clone; the tests read this one).
    #[cfg_attr(not(test), allow(dead_code))]
    dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Registry {
    fn new(cap: u64, ttl: std::time::Duration) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (listener_live, listener_dropped) = (live.clone(), dropped.clone());
        let cache = Cache::builder()
            .max_capacity(cap)
            .time_to_live(ttl)
            .eviction_listener(move |_key, _value, cause| {
                use moka::notification::RemovalCause::{Expired, Size};
                match cause {
                    Expired => {
                        listener_live.fetch_sub(1, Relaxed);
                    }
                    Size => {
                        listener_live.fetch_sub(1, Relaxed);
                        let n = listener_dropped.fetch_add(1, Relaxed) + 1;
                        super::s3::LISTING_FACTS_QUEUE_DROPPED
                            .with_label_values(&["unwritten"])
                            .inc();
                        if n == 1 || n.is_multiple_of(UNWRITTEN_LOG_EVERY) {
                            tracing::warn!(
                                "the registry of refused listing-facts writes is full ({cap} \
                                 entries): {n} entries dropped so far; those objects list their \
                                 stored size on other nodes until a HEAD or the healer writes \
                                 their facts"
                            );
                        }
                    }
                    _ => {}
                }
            })
            .build();
        Registry {
            cache,
            live,
            dropped,
        }
    }

    fn mark(&self, key: String, value: Unwritten) {
        use std::sync::atomic::Ordering::Relaxed;
        // `is_fresh` is true for exactly one of the threads that race to
        // insert the same key.
        if self
            .cache
            .entry(key.clone())
            .or_insert(value.clone())
            .is_fresh()
        {
            self.live.fetch_add(1, Relaxed);
        } else {
            self.cache.insert(key, value);
        }
    }

    fn any(&self) -> bool {
        self.live.load(std::sync::atomic::Ordering::Relaxed) > 0
    }

    fn remove(&self, key: &str) -> Option<Unwritten> {
        let entry = self.cache.remove(key)?;
        self.live.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        Some(entry)
    }
}

/// The registry key of a stored object: the object, not one of its versions.
fn object_key(scope: &str, bucket: &str, key: &str) -> String {
    format!("{scope}\0{bucket}\0{key}")
}

/// Note that the write of `facts_key` for the stored object `id` was refused.
pub fn mark_unwritten(id: &StoredObjectId<'_>, facts_key: String) {
    UNWRITTEN.mark(
        object_key(id.scope, id.bucket, id.key),
        Unwritten {
            etag: bare_etag(id.etag).to_string(),
            size: id.size,
            facts_key,
        },
    );
}

/// Are any facts writes refused and not offered again? A LIST asks first,
/// so that it costs nothing when the answer is no: one atomic load.
pub fn any_unwritten() -> bool {
    UNWRITTEN.any()
}

/// The facts key of a refused write of the stored object `id`, removed: the
/// caller offers it again (and marks it again if it is refused again). An
/// entry for another version of the object is dropped and gives `None`.
pub fn take_unwritten(id: &StoredObjectId<'_>) -> Option<String> {
    let entry = UNWRITTEN.remove(&object_key(id.scope, id.bucket, id.key))?;
    (entry.etag == bare_etag(id.etag) && entry.size == id.size).then_some(entry.facts_key)
}

/// The stored object was deleted, or written again with a write that went
/// into the queue: its refused write (if any) is moot.
pub fn forget_unwritten(scope: &str, bucket: &str, key: &str) {
    if UNWRITTEN.any() {
        UNWRITTEN.remove(&object_key(scope, bucket, key));
    }
}

#[cfg(test)]
mod unwritten_registry_tests {
    use super::*;

    fn entry(n: usize) -> (String, Unwritten) {
        (
            object_key("scope", "b", &format!("k{n}")),
            Unwritten {
                etag: "e".into(),
                size: 1,
                facts_key: format!("facts{n}"),
            },
        )
    }

    /// The count of the registry is exact, and a full registry drops its
    /// entries with a count in `deltaglider_listing_facts_queue_dropped_total
    /// {queue="unwritten"}`: every entry is live or dropped.
    #[test]
    fn a_full_registry_counts_every_entry_it_drops() {
        const CAP: u64 = 4;
        const N: usize = 40;
        let registry = Registry::new(CAP, UNWRITTEN_TTL);
        let metric = || {
            super::super::s3::LISTING_FACTS_QUEUE_DROPPED
                .with_label_values(&["unwritten"])
                .get()
        };
        let before = metric();
        assert!(!registry.any());
        for n in 0..N {
            let (key, value) = entry(n);
            registry.mark(key, value);
        }
        registry.cache.run_pending_tasks();
        let dropped = registry.dropped.load(std::sync::atomic::Ordering::Relaxed);
        let live = registry.live.load(std::sync::atomic::Ordering::Relaxed);
        assert!(dropped >= (N as u64 - CAP), "{dropped} dropped of {N}");
        assert_eq!(
            live as u64 + dropped,
            N as u64,
            "every entry is live or dropped"
        );
        assert_eq!(live as u64, registry.cache.entry_count());
        assert!(metric() - before >= dropped, "the metric counts the drops");
        assert!(registry.any() == (live > 0));
        // An explicit removal takes exactly one from the count.
        let kept = (0..N)
            .map(entry)
            .find(|(key, _)| registry.cache.contains_key(key))
            .expect("a kept entry");
        assert!(registry.remove(&kept.0).is_some());
        assert!(registry.remove(&kept.0).is_none());
        assert_eq!(
            registry.live.load(std::sync::atomic::Ordering::Relaxed),
            live - 1
        );
    }

    /// Marking an object again (a newer version refused) replaces its entry
    /// and does not count it twice.
    #[test]
    fn a_second_mark_of_one_object_counts_once() {
        let registry = Registry::new(100, UNWRITTEN_TTL);
        let (key, value) = entry(1);
        registry.mark(key.clone(), value);
        let newer = Unwritten {
            etag: "e2".into(),
            size: 2,
            facts_key: "facts-new".into(),
        };
        registry.mark(key.clone(), newer);
        assert_eq!(registry.live.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            registry.remove(&key).map(|u| u.facts_key).as_deref(),
            Some("facts-new")
        );
        assert!(!registry.any());
    }
}

/// The listing tokens that this process made for stored objects (issue
/// #120): a LIST right after a PUT finds the token here, before the
/// background writer has stored its `!!2` marker. Exact per stored object;
/// bounded. A token is opaque here: the encrypting wrapper opens it.
static TOKENS: LazyLock<Cache<String, String>> =
    LazyLock::new(|| Cache::builder().max_capacity(100_000).build());

/// Remember the listing token of the stored object `id`.
pub fn record_token(id: &StoredObjectId<'_>, token: &str) {
    if bare_etag(id.etag).is_empty() {
        return;
    }
    TOKENS.insert(cache_key(id), token.to_string());
}

/// The listing token this process made for the stored object `id`.
pub fn lookup_token(id: &StoredObjectId<'_>) -> Option<String> {
    TOKENS.get(&cache_key(id))
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
    /// The logical size is exact, the ETag is still the stored one: a
    /// sealed passthrough object listed without its listing token. Its
    /// stored size is the exact stored size of a DGE2 body, which gives the
    /// plaintext size (`encrypting::v2::plaintext_len_of`) without a
    /// request. Its logical ETag is known only to a HEAD (or a token).
    SizeOnly,
    /// Only the stored size is known.
    StoredOnly,
}

impl ListedSize {
    /// Is the logical SIZE known?
    pub fn is_known(self) -> bool {
        !matches!(self, ListedSize::StoredOnly)
    }

    /// Are both the logical size and the logical ETag known?
    pub fn is_exact(self) -> bool {
        matches!(self, ListedSize::Listed | ListedSize::Cached)
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
        assert_eq!(
            self_literals, 3,
            "`of`, `of_listing_token` (no metadata) and `ResolveListed::default`"
        );
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
