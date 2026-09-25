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
//! Everything here is pure; the S3 backend does the requests.

use super::list_size_cache::LogicalFacts;

/// The facts namespace at the root of every bucket. Its first segment is
/// `.dg`, which listings already hide as an internal directory.
pub const FACTS_ROOT: &str = ".dg/facts/";

/// Ends the encoded stored key inside a facts key.
const TERMINATOR: &str = "!!";

/// Payload format version, the first payload field.
const FORMAT: &str = "1";

/// S3's key length limit. A stored key too long for its facts key gets no
/// facts object and lists with its stored size.
const MAX_KEY_LEN: usize = 1024;

/// Is `key` in the facts namespace, and so never a user object?
pub fn is_facts_key(key: &str) -> bool {
    key.starts_with(FACTS_ROOT)
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

fn bare(etag: &str) -> &str {
    etag.trim_matches('"')
}

/// An ETag we can put in a key field: non-empty, and without the field
/// separator or characters that would change the key's meaning.
fn etag_fits(etag: &str) -> bool {
    !etag.is_empty() && etag.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
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
    let stored_etag = bare(stored_etag);
    let etag = bare(&facts.etag);
    if !etag_fits(stored_etag) || !etag_fits(etag) {
        return None;
    }
    if stored_size == facts.size && stored_etag == etag {
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
    if fields.next().is_some() || !etag_fits(&stored_etag) || !etag_fits(&etag) {
        return None;
    }
    Some(FactsEntry {
        stored_key: decode(enc_key)?,
        stored_etag,
        stored_size,
        facts: LogicalFacts { size, etag },
    })
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
    let stored_etag = bare(stored_etag);
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
        assert_eq!(facts_key("a.delta", "", 5, &facts(9, "e1")), None);
        assert_eq!(facts_key("a.delta", "e1", 5, &facts(9, "")), None);
        assert_eq!(facts_key("a.delta", "e.1", 5, &facts(9, "e2")), None);
        assert!(facts_key(&"k".repeat(1020), "e1", 5, &facts(9, "e2")).is_none());
    }

    #[test]
    fn foreign_keys_do_not_parse() {
        for key in [
            ".dg/facts/a.delta",
            ".dg/facts/a.delta!!2.e.1.2.e",
            ".dg/facts/a.delta!!1.e.x.2.e",
            ".dg/facts/a!z.delta!!1.e.1.2.e",
            ".dg/facts/a.delta!!1.e.1.2.e.extra",
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
}
