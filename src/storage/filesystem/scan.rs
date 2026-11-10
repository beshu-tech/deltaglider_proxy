// SPDX-License-Identifier: BUSL-1.1

//! Bounded directory scans for the filesystem backend's listings (F3, epic
//! #142).
//!
//! A directory can hold millions of entries and a client can ask for one key
//! of it, many times at once. A scan therefore never keeps the directory: it
//! reads it in sorted CHUNKS. A chunk is the smallest `chunk_size` entries
//! after a bound, chosen while `read_dir` streams (a bounded max-heap); the
//! next chunk is another pass after the last entry of this one. A request
//! holds one chunk per open directory, whatever the directory size. The cost
//! is one `read_dir` pass per chunk that the page reaches: a page of at most
//! 1,000 keys reaches one or two.

use super::{
    fsio, io_to_storage_error, note_held, stored_name, FilesystemBackend, StorageError, StoredName,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Entries of a directory that one scan holds at a time.
pub(super) const SCAN_CHUNK: usize = 1024;

#[cfg(test)]
thread_local! {
    /// A smaller chunk, so a test reaches the refill with a few entries.
    static CHUNK_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The chunk size now. Tests of the refill set a small one.
pub(super) fn chunk_size() -> usize {
    #[cfg(test)]
    if let Some(n) = CHUNK_OVERRIDE.with(|c| c.get()) {
        return n;
    }
    SCAN_CHUNK
}

#[cfg(test)]
pub(super) fn set_chunk_for_test(n: Option<usize>) {
    CHUNK_OVERRIDE.with(|c| c.set(n));
}

/// A directory entry that can be a key: a data file or a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WalkEntry {
    /// The name on disk.
    pub(super) name: String,
    /// The name in the key: a file's key name (`x` for `x.delta`, empty for
    /// the folder marker), a directory's own name.
    pub(super) key_name: String,
    pub(super) is_dir: bool,
    /// A data file is the delta form (`x.delta`).
    pub(super) is_delta: bool,
}

/// Where an entry sorts among its siblings (and the tie-break).
pub(super) type Order = (String, String);

impl WalkEntry {
    /// Where the entry sorts among its siblings: every key below a
    /// directory starts with `name/`.
    pub(super) fn order(&self) -> Order {
        let sort = if self.is_dir {
            format!("{}/", self.key_name)
        } else {
            self.key_name.clone()
        };
        (sort, self.name.clone())
    }
}

/// An entry with its order, so a heap compares by it.
struct Ranked(Order, WalkEntry);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

/// Pure: the smallest `limit` entries that sort after `after`, in order, and
/// whether more remain. Holds at most `limit + 1` entries however many it is
/// offered: the whole point of the module.
pub(super) struct Smallest {
    limit: usize,
    after: Option<Order>,
    heap: BinaryHeap<Ranked>,
    more: bool,
}

impl Smallest {
    pub(super) fn new(limit: usize, after: Option<Order>) -> Self {
        Self {
            limit: limit.max(1),
            after,
            heap: BinaryHeap::new(),
            more: false,
        }
    }

    pub(super) fn offer(&mut self, entry: WalkEntry) {
        let order = entry.order();
        if self.after.as_ref().is_some_and(|a| order <= *a) {
            return;
        }
        self.heap.push(Ranked(order, entry));
        if self.heap.len() > self.limit {
            self.heap.pop();
            self.more = true;
        }
        note_held(self.heap.len());
    }

    pub(super) fn finish(self) -> (Vec<WalkEntry>, bool) {
        let mut ranked = self.heap.into_vec();
        ranked.sort();
        (ranked.into_iter().map(|r| r.1).collect(), self.more)
    }
}

/// Decides which entries of a directory a scan keeps: `(key of the
/// directory, entry)`. Dropped entries cost nothing but their `read_dir`
/// step, and are never held.
pub(super) type Keep<'a> = &'a (dyn Fn(&str, &WalkEntry) -> bool + Sync);

/// One directory of a sorted walk, read chunk by chunk.
pub(super) struct SortedDir {
    pub(super) path: PathBuf,
    /// The key of the directory (`a/b`; empty at the bucket root).
    pub(super) key: String,
    chunk: Vec<WalkEntry>,
    /// The next entry of the chunk.
    next: usize,
    /// Entries remain after the chunk.
    more: bool,
}

impl SortedDir {
    /// The first chunk of the directory at `path`. Reads names and types
    /// only (no metadata). `None` when no directory is there.
    pub(super) async fn open(
        path: &Path,
        key: String,
        keep: Keep<'_>,
    ) -> Result<Option<Self>, StorageError> {
        let Some((chunk, more)) = read_chunk(path, &key, None, keep).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            path: path.to_path_buf(),
            key,
            chunk,
            next: 0,
            more,
        }))
    }

    /// The next entry in order, or `None` at the end. Reads the next chunk
    /// when this one is used up.
    pub(super) async fn next_entry(
        &mut self,
        keep: Keep<'_>,
    ) -> Result<Option<WalkEntry>, StorageError> {
        if self.next >= self.chunk.len() && self.more {
            let after = self.chunk.last().map(WalkEntry::order);
            match read_chunk(&self.path, &self.key, after, keep).await? {
                Some((chunk, more)) => {
                    self.chunk = chunk;
                    self.more = more;
                }
                // The directory went away between two chunks.
                None => {
                    self.chunk = Vec::new();
                    self.more = false;
                }
            }
            self.next = 0;
        }
        let entry = self.chunk.get(self.next).cloned();
        if entry.is_some() {
            self.next += 1;
        }
        Ok(entry)
    }

    fn held(&self) -> usize {
        self.chunk.len()
    }
}

/// One bounded pass over the directory: the smallest chunk of kept entries
/// after `after`.
async fn read_chunk(
    path: &Path,
    dir_key: &str,
    after: Option<Order>,
    keep: Keep<'_>,
) -> Result<Option<(Vec<WalkEntry>, bool)>, StorageError> {
    let mut dir = match fs::read_dir(path).await {
        Ok(dir) => dir,
        Err(e) if fsio::absent(&e) => return Ok(None),
        Err(e) => return Err(io_to_storage_error(e)),
    };
    let mut smallest = Smallest::new(chunk_size(), after);
    while let Some(entry) = dir.next_entry().await? {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let is_dir = match entry.file_type().await {
            Ok(ft) => ft.is_dir(),
            Err(e) if fsio::absent(&e) => continue, // deleted during the walk
            Err(e) => return Err(io_to_storage_error(e)),
        };
        let (key_name, is_delta) = if is_dir {
            (name.clone(), false)
        } else {
            match stored_name(&name) {
                StoredName::Object {
                    key_filename,
                    variant,
                } => (
                    key_filename.to_string(),
                    variant == super::super::traits::ObjectVariant::Delta,
                ),
                StoredName::Internal | StoredName::Reference => continue,
            }
        };
        let walk_entry = WalkEntry {
            name,
            key_name,
            is_dir,
            is_delta,
        };
        if keep(dir_key, &walk_entry) {
            smallest.offer(walk_entry);
        }
    }
    Ok(Some(smallest.finish()))
}

/// Join a directory key and a name: `a` + `b` = `a/b`; the root is empty.
pub(super) fn join_key(dir_key: &str, name: &str) -> String {
    if dir_key.is_empty() {
        name.to_string()
    } else {
        format!("{dir_key}/{name}")
    }
}

/// Pure: the common prefix that a delimiter makes of `key` under `prefix`:
/// `prefix` + the text up to the first delimiter + the delimiter. `None`
/// when `key` does not start with `prefix` or holds no delimiter after it.
/// For a directory's `below` (`a/b/`) the answer holds for every key under
/// it, because the delimiter lies inside `below`.
pub(super) fn collapse_key(key: &str, prefix: &str, delimiter: &str) -> Option<String> {
    let rest = key.strip_prefix(prefix)?;
    let pos = rest.find(delimiter)?;
    Some(format!("{prefix}{}{delimiter}", &rest[..pos]))
}

/// Pure: does the flat walk keep this entry? A file is kept when its key
/// starts with `prefix` and sorts after `token`; a directory when its
/// subtree may hold such a key. A `.dg` directory is the proxy's own and
/// never listed (F19).
///
/// `skip`: the common prefix just listed. Every key that starts with it is
/// collapsed already, so it is dropped while the directory is read; without
/// this a folder of N keys under one common prefix is read again for every
/// chunk (N²/chunk entries).
pub(super) fn flat_keep(
    prefix: &str,
    token: &str,
    skip: Option<&str>,
    dir_key: &str,
    entry: &WalkEntry,
) -> bool {
    if entry.is_dir {
        if entry.name == ".dg" {
            return false;
        }
        let below = format!("{}/", join_key(dir_key, &entry.key_name));
        if skip.is_some_and(|s| below.starts_with(s)) {
            return false;
        }
        super::flat_subtree_may_match(&below, prefix, token)
    } else {
        // A marker file at the bucket root names no key.
        let key = join_key(dir_key, &entry.key_name);
        !key.is_empty()
            && key.starts_with(prefix)
            && key.as_str() > token
            && !skip.is_some_and(|s| key.starts_with(s))
    }
}

/// What a flat walk yields.
pub(super) enum WalkStep {
    /// A data file: its key, path and name on disk.
    File {
        key: String,
        path: PathBuf,
        entry: WalkEntry,
    },
    /// With a delimiter: a whole subtree that collapses to one common
    /// prefix, and holds a visible key.
    Collapsed(String),
}

/// A sorted walk of the keys under a prefix, in key order, one chunk per
/// open directory.
pub(super) struct FlatWalk<'a> {
    stack: Vec<SortedDir>,
    prefix: &'a str,
    token: &'a str,
    delimiter: Option<&'a str>,
    /// The last common prefix yielded (see [`flat_keep`]).
    skip: Option<String>,
}

impl<'a> FlatWalk<'a> {
    /// `None` when the directory of the prefix does not exist.
    pub(super) async fn open(
        root: &Path,
        root_key: String,
        prefix: &'a str,
        token: &'a str,
        delimiter: Option<&'a str>,
    ) -> Result<Option<Self>, StorageError> {
        let keep = |d: &str, e: &WalkEntry| flat_keep(prefix, token, None, d, e);
        let Some(root) = SortedDir::open(root, root_key, &keep).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            stack: vec![root],
            prefix,
            token,
            delimiter,
            skip: None,
        }))
    }

    pub(super) async fn next(&mut self) -> Result<Option<WalkStep>, StorageError> {
        let (prefix, token, delimiter) = (self.prefix, self.token, self.delimiter);
        loop {
            let skip = self.skip.clone();
            let keep = |d: &str, e: &WalkEntry| flat_keep(prefix, token, skip.as_deref(), d, e);
            let Some(dir) = self.stack.last_mut() else {
                return Ok(None);
            };
            let Some(entry) = dir.next_entry(&keep).await? else {
                self.stack.pop();
                continue;
            };
            let key = join_key(&dir.key, &entry.key_name);
            let path = dir.path.join(&entry.name);
            // An entry of the chunk that read before the last common prefix
            // was listed.
            let below = if entry.is_dir {
                format!("{key}/")
            } else {
                key.clone()
            };
            if skip.as_deref().is_some_and(|s| below.starts_with(s)) {
                continue;
            }
            if !entry.is_dir {
                if let Some(cp) = delimiter.and_then(|d| collapse_key(&key, prefix, d)) {
                    self.skip = Some(cp.clone());
                    return Ok(Some(WalkStep::Collapsed(cp)));
                }
                return Ok(Some(WalkStep::File { key, path, entry }));
            }
            if let Some(cp) = delimiter.and_then(|d| collapse_key(&below, prefix, d)) {
                // Every key below collapses to `cp`: no need to read them,
                // only to know that one is there.
                if FilesystemBackend::dir_has_listed_key(&path).await? {
                    self.skip = Some(cp.clone());
                    return Ok(Some(WalkStep::Collapsed(cp)));
                }
                continue;
            }
            if let Some(sub) = SortedDir::open(&path, key, &keep).await? {
                self.stack.push(sub);
                note_held(self.stack.iter().map(SortedDir::held).sum());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn entry(name: &str, is_dir: bool) -> WalkEntry {
        WalkEntry {
            name: name.to_string(),
            key_name: name.trim_end_matches(".delta").to_string(),
            is_dir,
            is_delta: name.ends_with(".delta"),
        }
    }

    proptest! {
        /// Chunk after chunk, `Smallest` gives the entries in sorted order
        /// with none lost or repeated, for any chunk size; it never holds
        /// more than a chunk and one.
        #[test]
        fn chunks_concatenate_to_the_sorted_directory(
            names in proptest::collection::btree_set("[a-c.-]{1,3}(\\.delta)?", 0..40),
            dirs in proptest::collection::btree_set("[a-c.-]{1,3}", 0..10),
            chunk in 1usize..9,
        ) {
            let mut all: Vec<WalkEntry> = names.iter().map(|n| entry(n, false)).collect();
            all.extend(dirs.iter().map(|n| entry(n, true)));
            let mut want = all.clone();
            want.sort_by_key(WalkEntry::order);
            let mut got = Vec::new();
            let mut after: Option<Order> = None;
            loop {
                let mut s = Smallest::new(chunk, after.clone());
                for e in &all {
                    s.offer(e.clone());
                    prop_assert!(s.heap.len() <= chunk + 1);
                }
                let (part, more) = s.finish();
                prop_assert!(part.len() <= chunk);
                after = part.last().map(WalkEntry::order);
                got.extend(part);
                if !more {
                    break;
                }
            }
            prop_assert_eq!(got, want);
        }
    }

    /// The text of `async fn <name>` (or `fn`) up to its closing brace at
    /// the given indent.
    fn body_of<'a>(src: &'a str, name: &str, indent: &str) -> &'a str {
        let start = src
            .find(&format!("fn {name}"))
            .unwrap_or_else(|| panic!("fn {name} is gone"));
        let rest = &src[start..];
        let end = rest.find(&format!("\n{indent}}}\n")).expect("end of fn");
        &rest[..end]
    }

    /// F3 (epic #142): listing code never collects a `read_dir`. The only
    /// pass over a directory is `read_chunk`, which offers each entry to a
    /// bounded `Smallest`; every list function goes through `SortedDir` /
    /// `FlatWalk`. A `Vec` of the entries, a `collect()` or a direct
    /// `read_dir` in these functions brings the O(directory) memory back.
    #[test]
    fn listing_code_never_collects_a_read_dir() {
        let scan_src = include_str!("scan.rs");
        let read_chunk = body_of(scan_src, "read_chunk", "");
        assert!(read_chunk.contains("Smallest::new"), "{read_chunk}");
        for banned in ["entries.push", ".collect", "to_vec()", "Vec::with_capacity"] {
            assert!(!read_chunk.contains(banned), "read_chunk uses `{banned}`");
        }
        let mod_src = include_str!("mod.rs");
        for (name, indent) in [
            ("list_flat_page", "    "),
            ("list_objects_delegated", "    "),
            ("list_collapsed_page", "    "),
        ] {
            let body = body_of(mod_src, name, indent);
            for banned in [
                "read_dir(",
                ".collect",
                "SortedDir::read",
                "bulk_walk_recursive",
            ] {
                assert!(!body.contains(banned), "{name} uses `{banned}`");
            }
        }
    }

    #[test]
    fn a_delimiter_makes_a_common_prefix_of_a_key_or_a_whole_folder() {
        assert_eq!(collapse_key("a_b", "", "_"), Some("a_".into()));
        assert_eq!(
            collapse_key("pre/a_b_c", "pre/", "_"),
            Some("pre/a_".into())
        );
        assert_eq!(collapse_key("pre/x", "pre/", "_"), None);
        assert_eq!(collapse_key("other/a_b", "pre/", "_"), None);
        // The folder `a_b/` collapses everything below it.
        assert_eq!(collapse_key("a_b/", "", "_"), Some("a_".into()));
        // The delimiter reaches into the prefix: it is not found after it.
        assert_eq!(collapse_key("a_b", "a_", "_"), None);
        assert_eq!(collapse_key("a/b/c", "", "/b/"), Some("a/b/".into()));
    }
}
