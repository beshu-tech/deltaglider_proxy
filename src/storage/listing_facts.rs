// SPDX-License-Identifier: BUSL-1.1

//! Listing facts: the logical size and ETag of a stored object, kept where
//! an S3 LIST can see them (review C3).
//!
//! A LIST on an S3 backend reports the STORED object: the `.delta` of a
//! delta, the ciphertext of a proxy-encrypted object. The logical facts live
//! in the object's user metadata, which a LIST does not return, and a HEAD
//! per listed key is too slow (issue #82). The in-process
//! [`list_size_cache`](super::list_size_cache) helps only the process that
//! wrote or read the object.
//!
//! So every PUT whose stored facts differ from the logical ones also writes a
//! zero-byte FACTS OBJECT whose KEY carries the facts:
//!
//! ```text
//! .dg/facts/<enc(stored key)>!!1.<stored etag>.<stored size>.<size>.<etag>
//! ```
//!
//! A LIST page then reads the facts for its stored keys with one more LIST
//! (the facts sort in the same order as their stored keys), on any node and
//! after a restart. A PUT writes one small object and never rewrites an
//! index. A facts entry applies only to the stored object with exactly that
//! key, ETag and size, so a stale entry (the object was overwritten or
//! deleted) never matches: it is garbage, not a wrong answer.
//!
//! `enc` keeps key order: bytes `0x00..=0x21` become `!` + `(0x22 + b)`, and
//! every other byte stays. `!!` never occurs in `enc(key)`, so it ends the key
//! part, and it sorts below every continuation of a longer key. This makes
//! `facts(a) < facts(b)` exactly when `a < b`, and `/` stays `/`, so a
//! delimiter listing of the facts mirrors the listing of the objects.
//!
//! A sealed (format v2) object has no logical facts in clear. For it the
//! encrypting wrapper hands the backend an opaque LISTING TOKEN, kept the
//! same way:
//!
//! ```text
//! .dg/facts/<enc(stored key)>!!2.<stored etag>.<stored size>.<token>
//! ```
//!
//! The token is base64url (no dots); only the wrapper can open it. Both
//! shapes share the namespace, the scan, the delete cleanup and the GC.
//!
//! Everything here is pure; the S3 backend does the requests.

use super::list_size_cache::LogicalFacts;

/// The facts namespace at the root of every bucket. Its first segment is
/// `.dg`, which listings already hide as an internal directory.
pub const FACTS_ROOT: &str = ".dg/facts/";

/// The key claim of the bucket (`storage/encrypting/sentinel.rs`). It is in
/// the facts namespace so that listings hide it and the purge before a
/// DeleteBucket removes it. It is not a facts key that parses (the stored
/// key part is empty, and `key-claim` is no payload format), so the scans
/// and the GC leave it alone.
pub const KEY_CLAIM_KEY: &str = ".dg/facts/!!key-claim";

/// Ends the encoded stored key inside a facts key.
const TERMINATOR: &str = "!!";

/// Payload format version, the first payload field.
const FORMAT: &str = "1";

/// Payload format version of a listing token marker.
const TOKEN_FORMAT: &str = "2";

/// S3's key length limit. A stored key too long for its facts key gets no
/// facts object and lists with its stored size.
const MAX_KEY_LEN: usize = 1024;

/// Is `key` in the facts namespace, and so never a user object?
pub fn is_facts_key(key: &str) -> bool {
    key.starts_with(FACTS_ROOT)
}

/// A start-after past every facts key (`0` follows `/`).
pub const PAST_FACTS: &str = ".dg/facts0";

/// Where the next upstream page of a listing starts when the page it just
/// read (truncated) ends at `last_key`. `.` sorts before digits and letters,
/// so the facts come first in a listing from the root: one upstream page per
/// 1000 facts before the first user key. A page that ends inside the
/// namespace jumps past it instead of following the continuation token.
pub fn skip_past_facts(last_key: &str) -> Option<&'static str> {
    is_facts_key(last_key).then_some(PAST_FACTS)
}

/// Is a CommonPrefix of a listing inside the facts namespace (or a partial
/// path to it, like `.dg/fac` with delimiter `t`)? Never shown to a client.
pub fn is_internal_common_prefix(p: &str) -> bool {
    p.starts_with(FACTS_ROOT) || (p.len() > ".dg/".len() && FACTS_ROOT.starts_with(p))
}

/// Order-preserving, prefix-free encoding of a stored key (see module doc).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        if (ch as u32) <= 0x21 {
            out.push('!');
            out.push(char::from(0x22 + ch as u8));
        } else {
            out.push(ch);
        }
    }
    out
}

fn decode(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '!' {
            let c = chars.next()? as u32;
            if !(0x22..=0x43).contains(&c) {
                return None;
            }
            out.push(char::from((c - 0x22) as u8));
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

/// An ETag we can put in a key field: non-empty, and without the field
/// separator or characters that would change the key's meaning.
fn etag_fits(etag: &str) -> bool {
    !etag.is_empty() && etag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A logical ETag we can put in the last key field: one that fits, or the
/// empty ETag (`""`) that a HEAD and a GET send for a delta without
/// `dg-md5` (written by an older toolchain). The entry then lists that
/// same ETag. The empty field is the last one, and a v2.0.9 reader does not
/// parse such a key: it ignores the entry (a miss), never misreads it.
fn logical_etag_fits(etag: &str) -> bool {
    etag.is_empty() || etag_fits(etag)
}

/// The facts key that describes the stored object `stored_key` (with that
/// stored ETag and size). `None` when the object needs none (its stored facts
/// are the logical ones) or cannot have one (unknown ETag, key too long).
pub fn facts_key(
    stored_key: &str,
    stored_etag: &str,
    stored_size: u64,
    facts: &LogicalFacts,
) -> Option<String> {
    if stored_size == facts.size
        && super::list_size_cache::bare_etag(stored_etag)
            == super::list_size_cache::bare_etag(&facts.etag)
    {
        return None;
    }
    asked_facts_key(stored_key, stored_etag, stored_size, facts)
}

/// [`facts_key`], also when the stored facts are the logical ones. For an
/// object that a LIST asked about: behind a proxy-encryption key a LIST
/// cannot tell a plain object from a ciphertext, so it reads the facts of
/// every passthrough, and an object without an entry is a miss on every
/// LIST. `None` when it cannot have one (unknown ETag, key too long).
pub fn asked_facts_key(
    stored_key: &str,
    stored_etag: &str,
    stored_size: u64,
    facts: &LogicalFacts,
) -> Option<String> {
    let stored_etag = super::list_size_cache::bare_etag(stored_etag);
    let etag = super::list_size_cache::bare_etag(&facts.etag);
    if !etag_fits(stored_etag) || !logical_etag_fits(etag) {
        return None;
    }
    let key = format!(
        "{}{TERMINATOR}{FORMAT}.{stored_etag}.{stored_size}.{}.{etag}",
        stored_prefix(stored_key),
        facts.size
    );
    (key.len() <= MAX_KEY_LEN).then_some(key)
}

/// The prefix that every facts key of `stored_key` (and no other stored key)
/// starts with. Listing it finds the entries to clean up.
pub fn stored_prefix(stored_key: &str) -> String {
    format!("{FACTS_ROOT}{}", encode(stored_key))
}

/// One parsed facts key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsEntry {
    pub stored_key: String,
    pub stored_etag: String,
    pub stored_size: u64,
    pub facts: LogicalFacts,
}

/// Parse a facts key; `None` for anything that is not one in this format.
pub fn parse_facts_key(key: &str) -> Option<FactsEntry> {
    let rest = key.strip_prefix(FACTS_ROOT)?;
    let (enc_key, payload) = rest.split_once(TERMINATOR)?;
    let mut fields = payload.split('.');
    if fields.next()? != FORMAT {
        return None;
    }
    let stored_etag = fields.next()?.to_string();
    let stored_size = fields.next()?.parse().ok()?;
    let size = fields.next()?.parse().ok()?;
    let etag = fields.next()?.to_string();
    if fields.next().is_some() || !etag_fits(&stored_etag) || !logical_etag_fits(&etag) {
        return None;
    }
    Some(FactsEntry {
        stored_key: decode(enc_key)?,
        stored_etag,
        stored_size,
        facts: LogicalFacts { size, etag },
    })
}

/// A listing token we can put in the last key field: non-empty base64url
/// (no `.`, the field separator).
fn token_fits(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The marker key that holds the opaque listing `token` of the stored
/// object `stored_key` (with that stored ETag and size). `None` when it
/// cannot have one: unknown or odd ETag, a token that is not base64url, or
/// a key past S3's 1024-byte limit (the object then lists without a token).
pub fn token_key(
    stored_key: &str,
    stored_etag: &str,
    stored_size: u64,
    token: &str,
) -> Option<String> {
    let stored_etag = super::list_size_cache::bare_etag(stored_etag);
    if !etag_fits(stored_etag) || !token_fits(token) {
        return None;
    }
    let key = format!(
        "{}{stored_etag}.{stored_size}.{token}",
        token_prefix(stored_key)
    );
    (key.len() <= MAX_KEY_LEN).then_some(key)
}

/// The prefix that every token marker of `stored_key` (and no other marker)
/// starts with. A new token replaces the others under it.
pub fn token_prefix(stored_key: &str) -> String {
    format!("{}{TERMINATOR}{TOKEN_FORMAT}.", stored_prefix(stored_key))
}

/// One parsed token marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenEntry {
    pub stored_key: String,
    pub stored_etag: String,
    pub stored_size: u64,
    pub token: String,
}

/// Parse a token marker; `None` for anything that is not one.
pub fn parse_token_key(key: &str) -> Option<TokenEntry> {
    let rest = key.strip_prefix(FACTS_ROOT)?;
    let (enc_key, payload) = rest.split_once(TERMINATOR)?;
    let mut fields = payload.split('.');
    if fields.next()? != TOKEN_FORMAT {
        return None;
    }
    let stored_etag = fields.next()?.to_string();
    let stored_size = fields.next()?.parse().ok()?;
    let token = fields.next()?.to_string();
    if fields.next().is_some() || !etag_fits(&stored_etag) || !token_fits(&token) {
        return None;
    }
    Some(TokenEntry {
        stored_key: decode(enc_key)?,
        stored_etag,
        stored_size,
        token,
    })
}

/// Any marker of the facts namespace this release knows: `!!1` facts or a
/// `!!2` listing token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    Facts(FactsEntry),
    Token(TokenEntry),
}

impl Marker {
    /// The stored object the marker describes: `(key, ETag, size)`.
    pub fn stored(&self) -> (&str, &str, u64) {
        match self {
            Marker::Facts(e) => (&e.stored_key, &e.stored_etag, e.stored_size),
            Marker::Token(e) => (&e.stored_key, &e.stored_etag, e.stored_size),
        }
    }

    pub fn stored_key(&self) -> &str {
        self.stored().0
    }
}

/// Parse a marker of either shape. The GC and the delete cleanup treat both
/// alike: a marker goes with the stored object it describes.
pub fn parse_marker(key: &str) -> Option<Marker> {
    parse_facts_key(key)
        .map(Marker::Facts)
        .or_else(|| parse_token_key(key).map(Marker::Token))
}

/// The listing token that applies to one listed stored object. Only a token
/// with the same stored ETag and size applies; two such tokens that
/// disagree apply neither.
pub fn token_for<'a>(
    entries: impl IntoIterator<Item = &'a TokenEntry>,
    stored_etag: &str,
    stored_size: u64,
) -> Option<String> {
    let stored_etag = super::list_size_cache::bare_etag(stored_etag);
    let mut found: Option<&str> = None;
    for e in entries {
        if e.stored_etag != stored_etag || e.stored_size != stored_size {
            continue;
        }
        match found {
            Some(t) if t != e.token => return None,
            _ => found = Some(&e.token),
        }
    }
    found.map(str::to_string)
}

/// One facts object the garbage collection read: its key, its parsed form
/// (`None`: not in a format this release knows) and its server
/// `LastModified`.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub key: String,
    pub entry: Option<Marker>,
    pub modified: Option<i64>,
}

/// Pure: the facts objects of one GC page to delete. An entry goes when the
/// stored object it describes (that key, stored ETag and size) is not in
/// `live`, the listing of the stored keys, which is complete up to and with
/// `verified_through`. The delete cleanup is best effort (an in-memory
/// queue, lost on a crash; a same-second entry is kept on purpose), so
/// without this such entries stayed forever.
///
/// Kept: an entry younger than `grace_secs` (its PUT, or a rewrite on
/// another node, may be in flight), an entry past the verified range, and
/// anything not in this format (a newer release's entries).
pub fn gc_doomed(
    candidates: &[GcCandidate],
    live: &std::collections::HashMap<String, (String, u64)>,
    verified_through: Option<&str>,
    now: i64,
    grace_secs: i64,
) -> Vec<String> {
    let Some(through) = verified_through else {
        return Vec::new();
    };
    candidates
        .iter()
        .filter(|c| {
            c.modified
                .is_some_and(|m| now.saturating_sub(m) >= grace_secs)
        })
        .filter_map(|c| Some((c, c.entry.as_ref()?.stored())))
        .filter(|(_, (key, _, _))| *key <= through)
        .filter(|(_, (key, stored_etag, stored_size))| {
            live.get(*key).is_none_or(|(etag, size)| {
                super::list_size_cache::bare_etag(etag) != *stored_etag || size != stored_size
            })
        })
        .map(|(c, _)| c.key.clone())
        .collect()
}

/// A start-after that lists `key` itself and what follows (S3 returns the
/// keys strictly after it): `key` without its last character. Keys between
/// the two cost a little more listing, never a missed key.
pub fn start_before(key: &str) -> String {
    let mut s = key.to_string();
    s.pop();
    s
}

/// The one LIST (in pages) that returns the facts of a set of stored keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsScan {
    pub prefix: String,
    /// `/` when every stored key sits in the same directory: the facts of
    /// deeper keys then fold into common prefixes and cost nothing.
    pub delimiter: Option<&'static str>,
    pub start_after: String,
    /// Every facts key of the stored keys sorts at or below this.
    pub last: String,
}

impl FactsScan {
    /// Is a listed key past every facts key this scan needs?
    pub fn is_past(&self, key: &str) -> bool {
        key > self.last.as_str()
    }
}

fn dir_of(key: &str) -> &str {
    key.rfind('/').map_or("", |i| &key[..=i])
}

/// Plan the facts scan for these stored keys (any order). `None` for none.
pub fn plan_facts_scan<'a>(stored_keys: impl IntoIterator<Item = &'a str>) -> Option<FactsScan> {
    let mut keys: Vec<&str> = stored_keys.into_iter().collect();
    keys.sort_unstable();
    let (first, last) = (*keys.first()?, *keys.last()?);
    let dir = dir_of(first);
    let (prefix, delimiter) = if keys.iter().all(|k| dir_of(k) == dir) {
        (dir.to_string(), Some("/"))
    } else {
        // The longest common prefix of the sorted keys is that of the ends.
        let common = first
            .char_indices()
            .zip(last.chars())
            .find(|((_, a), b)| a != b)
            .map_or(first.len().min(last.len()), |((i, _), _)| i);
        (first[..common].to_string(), None)
    };
    Some(FactsScan {
        prefix: format!("{FACTS_ROOT}{}", encode(&prefix)),
        delimiter,
        start_after: stored_prefix(first),
        // Payload characters are ASCII below DEL.
        last: format!("{}{TERMINATOR}\u{7f}", stored_prefix(last)),
    })
}

/// The logical facts that apply to one listed stored object, from the facts
/// entries read for its key. Only an entry with the same stored ETag and
/// size applies; two such entries that disagree apply neither.
pub fn facts_for<'a>(
    entries: impl IntoIterator<Item = &'a FactsEntry>,
    stored_etag: &str,
    stored_size: u64,
) -> Option<LogicalFacts> {
    let stored_etag = super::list_size_cache::bare_etag(stored_etag);
    let mut found: Option<&LogicalFacts> = None;
    for e in entries {
        if e.stored_etag != stored_etag || e.stored_size != stored_size {
            continue;
        }
        match found {
            Some(f) if f != &e.facts => return None,
            _ => found = Some(&e.facts),
        }
    }
    found.cloned()
}

/// How a batch cleanup finds the facts entries of deleted stored keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupRead {
    /// Few keys: one prefix LIST per key.
    PerKey(Vec<String>),
    /// Many keys in one directory: one range scan of that directory, at most
    /// `pages` pages; the keys the scan does not reach fall back to
    /// `PerKey`.
    Range {
        scan: FactsScan,
        keys: Vec<String>,
        pages: usize,
    },
}

/// Keys at or below this count in one directory read per key.
const PER_KEY_MAX: usize = 2;

/// Pure: group deleted stored keys by directory and pick the cheapest read
/// for each group. A batch of `n` keys in one directory costs about
/// `n / 1000 + 1` LIST requests instead of `n`.
pub fn plan_cleanup(stored_keys: impl IntoIterator<Item = String>) -> Vec<CleanupRead> {
    let mut by_dir: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for k in stored_keys {
        by_dir.entry(dir_of(&k).to_string()).or_default().insert(k);
    }
    by_dir
        .into_values()
        .map(|keys| {
            let keys: Vec<String> = keys.into_iter().collect();
            if keys.len() <= PER_KEY_MAX {
                return CleanupRead::PerKey(keys);
            }
            let scan = plan_facts_scan(keys.iter().map(String::as_str))
                .expect("a non-empty group has a scan");
            CleanupRead::Range {
                pages: keys.len() / 1000 + 2,
                scan,
                keys,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(size: u64, etag: &str) -> LogicalFacts {
        LogicalFacts {
            size,
            etag: etag.into(),
        }
    }

    #[test]
    fn a_facts_key_round_trips() {
        let key = facts_key(
            "rel/v1/app zip!.tar.delta",
            "\"d41d8cd98f00b204e9800998ecf8427e\"",
            46,
            &facts(3_000_000, "abc-3"),
        )
        .unwrap();
        assert!(is_facts_key(&key));
        assert!(key.starts_with(".dg/facts/rel/v1/app"));
        assert_eq!(
            parse_facts_key(&key).unwrap(),
            FactsEntry {
                stored_key: "rel/v1/app zip!.tar.delta".into(),
                stored_etag: "d41d8cd98f00b204e9800998ecf8427e".into(),
                stored_size: 46,
                facts: facts(3_000_000, "abc-3"),
            }
        );
    }

    #[test]
    fn objects_that_list_their_own_facts_get_none() {
        assert_eq!(facts_key("a.txt", "e1", 5, &facts(5, "e1")), None);
        // Unless a LIST asked: then the entry says "the stored facts are
        // the logical ones".
        let asked = asked_facts_key("a.txt", "\"e1\"", 5, &facts(5, "e1")).unwrap();
        assert_eq!(parse_facts_key(&asked).unwrap().facts, facts(5, "e1"));
        assert_eq!(asked_facts_key("a.txt", "", 5, &facts(5, "")), None);
        assert_eq!(facts_key("a.delta", "", 5, &facts(9, "e1")), None);
        // A delta without `dg-md5`: its HEAD sends the empty ETag, and so
        // does its entry.
        let no_md5 = facts_key("a.delta", "e1", 5, &facts(9, "")).unwrap();
        assert!(no_md5.ends_with("!!1.e1.5.9."), "{no_md5}");
        assert_eq!(parse_facts_key(&no_md5).unwrap().facts, facts(9, ""));
        assert_eq!(facts_key("a.delta", "e.1", 5, &facts(9, "e2")), None);
        assert!(facts_key(&"k".repeat(1020), "e1", 5, &facts(9, "e2")).is_none());
    }

    #[test]
    fn a_token_key_round_trips() {
        let token = "AbC-_09xyz";
        let key = token_key("rel/v1/app zip!.tar.delta", "\"e1-3\"", 46, token).unwrap();
        assert!(is_facts_key(&key));
        assert!(key.starts_with(&token_prefix("rel/v1/app zip!.tar.delta")));
        assert!(key.ends_with("!!2.e1-3.46.AbC-_09xyz"), "{key}");
        let entry = TokenEntry {
            stored_key: "rel/v1/app zip!.tar.delta".into(),
            stored_etag: "e1-3".into(),
            stored_size: 46,
            token: token.into(),
        };
        assert_eq!(parse_token_key(&key), Some(entry.clone()));
        assert_eq!(parse_marker(&key), Some(Marker::Token(entry)));
        // A token marker is not a facts key, and the reverse.
        assert_eq!(parse_facts_key(&key), None);
        let facts = facts_key("a.delta", "e1", 5, &facts(9, "m")).unwrap();
        assert_eq!(parse_token_key(&facts), None);
        assert!(matches!(parse_marker(&facts), Some(Marker::Facts(_))));
        // The token prefix covers the tokens of exactly this stored key.
        assert!(!token_key("rel/a.delta.x", "e1", 1, "t")
            .unwrap()
            .starts_with(&token_prefix("rel/a.delta")));
    }

    #[test]
    fn a_token_that_cannot_be_a_key_gets_none() {
        assert_eq!(token_key("a.delta", "", 5, "t"), None, "no ETag");
        assert_eq!(token_key("a.delta", "e.1", 5, "t"), None, "dotted ETag");
        assert_eq!(token_key("a.delta", "e1", 5, ""), None, "empty token");
        assert_eq!(token_key("a.delta", "e1", 5, "a.b"), None, "dotted token");
        assert_eq!(
            token_key("a.delta", "e1", 5, "a+b/c="),
            None,
            "not base64url"
        );
        // S3's 1024-byte key limit: the write is skipped.
        let fits = token_key(&"k".repeat(900), "e1", 5, &"t".repeat(50));
        assert!(fits.is_some_and(|k| k.len() <= MAX_KEY_LEN));
        assert_eq!(token_key(&"k".repeat(1000), "e1", 5, &"t".repeat(50)), None);
        for key in [
            ".dg/facts/a.delta!!2.e.1",
            ".dg/facts/a.delta!!2.e.1.",
            ".dg/facts/a.delta!!2.e.x.t",
            ".dg/facts/a.delta!!2.e.1.t.u",
            ".dg/facts/a.delta!!2..1.t",
            ".dg/facts/a.delta!!3.e.1.t",
            "a.delta!!2.e.1.t",
        ] {
            assert_eq!(parse_token_key(key), None, "{key}");
        }
    }

    /// Only a token of the same stored object applies; two that disagree
    /// apply neither.
    #[test]
    fn a_token_applies_only_to_the_same_stored_object() {
        let entry = |etag: &str, size: u64, token: &str| TokenEntry {
            stored_key: "k".into(),
            stored_etag: etag.into(),
            stored_size: size,
            token: token.into(),
        };
        let a = entry("e1", 10, "t1");
        let stale = entry("e0", 10, "t0");
        assert_eq!(token_for([&a, &stale], "\"e1\"", 10), Some("t1".into()));
        assert_eq!(token_for([&a], "e1", 11), None);
        assert_eq!(token_for([&stale], "e1", 10), None);
        assert_eq!(token_for([&a, &a.clone()], "e1", 10), Some("t1".into()));
        assert_eq!(token_for([&a, &entry("e1", 10, "t2")], "e1", 10), None);
    }

    proptest::proptest! {
        /// Token markers sort and scan like the facts of the same keys.
        #[test]
        fn a_scan_covers_the_tokens_of_its_keys(
            keys in proptest::collection::vec("[ !a-c/]{1,6}", 1..6),
        ) {
            let scan = plan_facts_scan(keys.iter().map(String::as_str)).unwrap();
            for k in &keys {
                let tk = token_key(k, "e1", 1, "tok_-9").unwrap();
                proptest::prop_assert!(tk.starts_with(&scan.prefix), "{tk} {scan:?}");
                proptest::prop_assert!(tk > scan.start_after, "{tk} {scan:?}");
                proptest::prop_assert!(!scan.is_past(&tk), "{tk} {scan:?}");
                proptest::prop_assert_eq!(
                    parse_marker(&tk).map(|m| m.stored_key().to_string()),
                    Some(k.clone())
                );
            }
        }
    }

    #[test]
    fn foreign_keys_do_not_parse() {
        for key in [
            ".dg/facts/a.delta",
            ".dg/facts/a.delta!!2.e.1.2.e",
            ".dg/facts/a.delta!!1.e.x.2.e",
            ".dg/facts/a!z.delta!!1.e.1.2.e",
            ".dg/facts/a.delta!!1.e.1.2.e.extra",
            ".dg/facts/a.delta!!1..1.2.e",
            ".dg/facts/a.delta!!1.e.1.2.\"\"",
            "a.delta!!1.e.1.2.e",
        ] {
            assert_eq!(parse_facts_key(key), None, "{key}");
        }
    }

    /// Only an entry for the same stored object applies; two that disagree
    /// apply neither.
    #[test]
    fn facts_apply_only_to_the_same_stored_object() {
        let entry = |etag: &str, size: u64, f: LogicalFacts| FactsEntry {
            stored_key: "k".into(),
            stored_etag: etag.into(),
            stored_size: size,
            facts: f,
        };
        let a = entry("e1", 10, facts(100, "m1"));
        let stale = entry("e0", 10, facts(90, "m0"));
        assert_eq!(
            facts_for([&a, &stale], "\"e1\"", 10),
            Some(facts(100, "m1"))
        );
        assert_eq!(facts_for([&a], "e1", 11), None);
        assert_eq!(facts_for([&stale], "e1", 10), None);
        let other = entry("e1", 10, facts(101, "m2"));
        assert_eq!(
            facts_for([&a, &a.clone()], "e1", 10),
            Some(facts(100, "m1"))
        );
        assert_eq!(facts_for([&a, &other], "e1", 10), None);
    }

    #[test]
    fn one_directory_scans_with_a_delimiter() {
        let scan = plan_facts_scan(["rel/b.zip.delta", "rel/a.zip.delta"]).unwrap();
        assert_eq!(scan.prefix, ".dg/facts/rel/");
        assert_eq!(scan.delimiter, Some("/"));
        assert_eq!(scan.start_after, ".dg/facts/rel/a.zip.delta");
        let scan = plan_facts_scan(["rel/a/x.delta", "rel/b/y.delta"]).unwrap();
        assert_eq!(scan.prefix, ".dg/facts/rel/");
        assert_eq!(scan.delimiter, None);
        let scan = plan_facts_scan(["a.delta", "b.delta"]).unwrap();
        assert_eq!(
            (scan.prefix.as_str(), scan.delimiter),
            (".dg/facts/", Some("/"))
        );
        assert!(plan_facts_scan([]).is_none());
    }

    #[test]
    fn a_batch_cleanup_scans_each_directory_once() {
        let many: Vec<String> = (0..1000).map(|i| format!("d/{i:04}.zip.delta")).collect();
        let mut keys = many.clone();
        keys.push("e/x.delta".into());
        keys.push("d/sub/y.delta".into());
        let plan = plan_cleanup(keys);
        assert_eq!(plan.len(), 3);
        match &plan[0] {
            CleanupRead::Range { scan, keys, pages } => {
                assert_eq!(scan.prefix, ".dg/facts/d/");
                assert_eq!(scan.delimiter, Some("/"));
                assert_eq!(keys.len(), 1000);
                assert_eq!(*pages, 3);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(plan[1], CleanupRead::PerKey(vec!["d/sub/y.delta".into()]));
        assert_eq!(plan[2], CleanupRead::PerKey(vec!["e/x.delta".into()]));
    }

    proptest::proptest! {
        /// `enc` keeps order and adds a terminator that sorts first, so the
        /// facts keys sort exactly like their stored keys.
        #[test]
        fn facts_keys_sort_like_their_stored_keys(
            a in "[ -#./0a-c~\u{e9}]{0,6}",
            b in "[ -#./0a-c~\u{e9}]{0,6}",
        ) {
            let f = facts(7, "m");
            let ka = facts_key(&a, "e1", 1, &f).unwrap();
            let kb = facts_key(&b, "e1", 1, &f).unwrap();
            proptest::prop_assert_eq!(a.cmp(&b), ka.cmp(&kb));
            proptest::prop_assert_eq!(parse_facts_key(&ka).unwrap().stored_key, a.clone());
            // The cleanup prefix of `a` covers only the facts of `a`.
            proptest::prop_assert_eq!(
                kb.starts_with(&format!("{}!!", stored_prefix(&a))),
                a == b
            );
        }

        /// Every facts key of the planned stored keys lies inside the scan:
        /// under its prefix, after `start_after`, not past `last`.
        #[test]
        fn a_scan_covers_the_facts_of_its_keys(
            keys in proptest::collection::vec("[ !a-c/]{1,6}", 1..6),
        ) {
            let scan = plan_facts_scan(keys.iter().map(String::as_str)).unwrap();
            for k in &keys {
                let fk = facts_key(k, "e1", 1, &facts(9, "m")).unwrap();
                proptest::prop_assert!(fk.starts_with(&scan.prefix), "{fk} {scan:?}");
                proptest::prop_assert!(fk > scan.start_after, "{fk} {scan:?}");
                proptest::prop_assert!(!scan.is_past(&fk), "{fk} {scan:?}");
                if let Some(d) = scan.delimiter {
                    // Same directory: no delimiter below the scan prefix.
                    proptest::prop_assert!(!fk[scan.prefix.len()..].contains(d));
                }
            }
        }
    }

    #[test]
    fn a_page_that_ends_in_the_facts_jumps_past_them() {
        let last = facts_key(
            "zz/o.zip.delta",
            "abc",
            10,
            &LogicalFacts {
                size: 100,
                etag: "def".into(),
            },
        )
        .unwrap();
        assert_eq!(skip_past_facts(&last), Some(PAST_FACTS));
        assert!(PAST_FACTS > last.as_str());
        // The jump skips no key outside the facts: the root reference and
        // every user key sort after it.
        assert!(PAST_FACTS < ".dg/reference.bin" && PAST_FACTS < "zz/a.txt");
        assert_eq!(skip_past_facts(".dg/reference.bin"), None);
        assert_eq!(skip_past_facts("a.txt"), None);
    }

    #[test]
    fn facts_common_prefixes_are_internal() {
        for p in [".dg/facts/", ".dg/facts/zz/", ".dg/fac", ".dg/facts"] {
            assert!(is_internal_common_prefix(p), "{p}");
        }
        for p in [".dg/", ".d", "", "a/.dg/facts/x/", ".well-known/", ".dgx/"] {
            assert!(!is_internal_common_prefix(p), "{p}");
        }
    }

    #[test]
    fn gc_deletes_only_old_verified_entries_of_gone_objects() {
        let facts = |k: &str, etag: &str, size: u64| {
            facts_key(
                k,
                etag,
                size,
                &LogicalFacts {
                    size: size * 10,
                    etag: "logical".into(),
                },
            )
            .unwrap()
        };
        let cand = |key: String, modified: i64| GcCandidate {
            entry: parse_marker(&key),
            key,
            modified: Some(modified),
        };
        let gone = facts("d/gone.delta", "e1", 1);
        let live_ok = facts("d/live.delta", "e2", 2);
        let stale = facts("d/over.delta", "old", 3);
        let fresh = facts("d/fresh.delta", "e4", 4);
        let far = facts("z/far.delta", "e5", 5);
        // Token markers go with their object exactly like facts.
        let token_gone = token_key("d/gone2.delta", "e6", 6, "tok").unwrap();
        let token_live = token_key("d/live.delta", "e2", 2, "tok").unwrap();
        let token_stale = token_key("d/over.delta", "old", 3, "tok").unwrap();
        let cands = vec![
            cand(gone.clone(), 0),
            cand(token_gone.clone(), 0),
            cand(live_ok.clone(), 0),
            cand(token_live.clone(), 0),
            cand(stale.clone(), 0),
            cand(token_stale.clone(), 0),
            cand(fresh.clone(), 99),
            cand(far.clone(), 0),
            GcCandidate {
                key: ".dg/facts/d/x!!2.future".into(),
                entry: None,
                modified: Some(0),
            },
        ];
        let live: std::collections::HashMap<String, (String, u64)> = [
            ("d/live.delta".to_string(), ("\"e2\"".to_string(), 2)),
            ("d/over.delta".to_string(), ("\"new\"".to_string(), 3)),
        ]
        .into_iter()
        .collect();
        let doomed = gc_doomed(&cands, &live, Some("d/zzz"), 100, 10);
        assert_eq!(doomed, vec![gone, token_gone, stale, token_stale]);
        assert!(gc_doomed(&cands, &live, None, 100, 10).is_empty());
        assert_eq!(start_before("d/a.delta"), "d/a.delt");
    }
}
