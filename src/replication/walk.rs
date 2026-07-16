// SPDX-License-Identifier: GPL-3.0-only

//! Fused directory-scoped reconcile walk — PURE core (no I/O, no tokio).
//!
//! The reconcile worker drives a tree walk over (source, destination)
//! directory pairs. This module owns every decision: ordering, resume,
//! watermark, per-directory diff. The async driver in `worker.rs` only
//! executes commands and feeds results back.
//!
//! Ordering foundation (proved by `proptests::sibling_subtree_bound`):
//! all paths are RELATIVE to the normalized rule prefixes and compared
//! byte-wise. Preorder DFS consuming children in full-prefix-string
//! order visits every item in global lexicographic order of relative
//! paths, so "everything ≤ watermark is settled" is a complete,
//! single-string resume cursor — even mid-directory.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// ContiguousTracker
// ---------------------------------------------------------------------------

/// Contiguous-progress tracker over relative paths.
///
/// `open` entries are unsettled work items plus per-listing frontier
/// markers (refcounted — an item and a marker may share a path). The
/// watermark is the largest settled path with every path ≤ it settled;
/// it advances only past the smallest open entry, so out-of-order
/// completion (parallel dirs, concurrent copies) can never overtake
/// undone work. Callers must never open a path ≤ the current watermark.
#[derive(Debug, Default)]
pub struct ContiguousTracker {
    open: BTreeMap<String, u32>,
    settled_ahead: BTreeSet<String>,
    watermark: Option<String>,
}

impl ContiguousTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track an unsettled path (work item or listing frontier marker).
    pub fn open(&mut self, path: &str) {
        debug_assert!(
            self.watermark.as_deref().is_none_or(|w| path > w),
            "opened path {path:?} not beyond watermark {:?}",
            self.watermark
        );
        *self.open.entry(path.to_string()).or_insert(0) += 1;
    }

    /// Settle a work item: its outcome is final and durable-equivalent.
    pub fn settle(&mut self, path: &str) {
        self.release(path);
        self.settled_ahead.insert(path.to_string());
        self.promote();
    }

    /// Remove a frontier marker without recording it as settled work.
    pub fn close(&mut self, path: &str) {
        self.release(path);
        self.promote();
    }

    /// Move a listing frontier marker forward (new page emitted up to `new`).
    /// Opens `new` BEFORE closing `old` so progress can never transiently
    /// promote past paths the stream still guards.
    pub fn advance_marker(&mut self, old: &str, new: &str) {
        debug_assert!(new > old, "marker must move forward: {old:?} -> {new:?}");
        self.open(new);
        self.close(old);
    }

    pub fn watermark(&self) -> Option<&str> {
        self.watermark.as_deref()
    }

    /// True when nothing is tracked as open (all work settled or closed).
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    fn release(&mut self, path: &str) {
        match self.open.get_mut(path) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                self.open.remove(path);
            }
            None => debug_assert!(false, "release of untracked path {path:?}"),
        }
    }

    fn promote(&mut self) {
        while let Some(first) = self.settled_ahead.first() {
            let blocked = self
                .open
                .first_key_value()
                .is_some_and(|(o, _)| o.as_str() <= first.as_str());
            if blocked {
                break;
            }
            let p = self.settled_ahead.pop_first().expect("checked non-empty");
            self.watermark = Some(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Resume rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// Entire subtree (or the object) is ≤ watermark ⇒ already settled.
    Skip,
    /// Dir is an ancestor of (or equal to) the watermark: its own listing
    /// completed, but descendants > watermark remain — re-list and apply
    /// the rule per child.
    DescendOnly,
    /// Strictly beyond the watermark — process fully.
    Process,
}

/// PURE resume decision for one relative path against watermark `w`.
/// Soundness rests on the sibling-subtree bound: a dir `d ≤ w` that is
/// not an ancestor of `w` has ALL descendants < w (first byte difference
/// lies inside `d`), hence settled.
pub fn resume_action(rel: &str, is_dir: bool, w: &str) -> ResumeAction {
    if is_dir && w.starts_with(rel) {
        ResumeAction::DescendOnly
    } else if rel <= w {
        ResumeAction::Skip
    } else {
        ResumeAction::Process
    }
}

/// Listing continuation token to resume dir `dir` (DescendOnly) so the
/// backend drops only provably-settled entries. Returns the child prefix
/// containing `w` MINUS its trailing slash: a strictly-greater backend
/// then still emits that child common-prefix (we must descend into it),
/// and an AWS `start_after` backend merely re-emits entries the resume
/// rule filters anyway. `None` ⇒ list from the beginning.
pub fn resume_list_token(dir: &str, w: &str) -> Option<String> {
    if !w.starts_with(dir) || w == dir {
        return None;
    }
    let rest = &w[dir.len()..];
    Some(match rest.find('/') {
        Some(i) => w[..dir.len() + i].to_string(),
        None => w.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Scope-stamped walk cursor persisted in `replication_state.continuation_token`.
/// `pos` is the watermark: a relative path (object or dir). Everything ≤ pos
/// is settled; the walk resumes via `resume_action` against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorV1 {
    pub v: u32,
    pub scope: String,
    pub pos: String,
}

/// Canonical scope stamp. Prefixes must already be rule-normalized.
pub fn cursor_scope(
    src_bucket: &str,
    src_prefix: &str,
    dst_bucket: &str,
    dst_prefix: &str,
) -> String {
    format!("{src_bucket}|{src_prefix}|{dst_bucket}|{dst_prefix}")
}

/// Parse a persisted cursor. Legacy flat listing tokens (non-JSON), version
/// or scope mismatches, and empty positions all mean "start fresh" — copies
/// are idempotent, so discarding an unusable cursor is always safe.
pub fn load_cursor(raw: Option<&str>, scope: &str) -> Option<CursorV1> {
    let c: CursorV1 = serde_json::from_str(raw?).ok()?;
    (c.v == 1 && c.scope == scope && !c.pos.is_empty()).then_some(c)
}

impl CursorV1 {
    pub fn new(scope: &str, pos: &str) -> Self {
        Self {
            v: 1,
            scope: scope.to_string(),
            pos: pos.to_string(),
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("cursor serializes")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_settles_in_order() {
        let mut t = ContiguousTracker::new();
        t.open("a");
        t.open("b");
        t.open("c");
        assert_eq!(t.watermark(), None);
        t.settle("b");
        assert_eq!(t.watermark(), None, "a still open blocks b");
        t.settle("a");
        assert_eq!(t.watermark(), Some("b"), "a+b contiguous");
        t.settle("c");
        assert_eq!(t.watermark(), Some("c"));
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_refcounts_shared_paths() {
        let mut t = ContiguousTracker::new();
        t.open("d/x"); // item
        t.open("d/x"); // marker parked at same path
        t.settle("d/x");
        assert_eq!(t.watermark(), None, "marker still guards d/x");
        t.close("d/x");
        assert_eq!(t.watermark(), Some("d/x"));
    }

    #[test]
    fn tracker_marker_advance_never_transiently_promotes() {
        let mut t = ContiguousTracker::new();
        t.open("d/"); // marker at dir
        t.open("d/a"); // emitted item
        t.settle("d/a");
        // Marker moves d/ -> d/a: settled d/a must NOT promote past the
        // marker's new position guard... it may promote exactly to d/a.
        t.advance_marker("d/", "d/a");
        assert_eq!(
            t.watermark(),
            None,
            "open marker AT d/a blocks settle of d/a"
        );
        t.close("d/a");
        assert_eq!(t.watermark(), Some("d/a"));
        assert!(t.is_empty());
    }

    #[test]
    fn tracker_empty_open_promotes_everything() {
        let mut t = ContiguousTracker::new();
        t.open("m/");
        t.open("m/x");
        t.open("m/y");
        t.settle("m/y");
        t.settle("m/x");
        t.close("m/");
        assert_eq!(t.watermark(), Some("m/y"));
    }

    #[test]
    fn resume_action_truth_table() {
        use ResumeAction::*;
        let cases: &[(&str, bool, &str, ResumeAction)] = &[
            // (rel, is_dir, watermark, expected)
            ("a-b/", true, "a/", Skip), // '-' < '/' ⇒ whole sibling done
            ("a/", true, "a//b/x", DescendOnly), // ancestor via empty segment
            ("a//", true, "a//b/x", DescendOnly),
            ("a//b/", true, "a//b/x", DescendOnly),
            ("a//a/", true, "a//b/x", Skip),     // sibling before w
            ("a//c/", true, "a//b/x", Process),  // sibling after w
            ("foo/", true, "foobar/", Skip),     // '/' < 'b' ⇒ foo/ subtree < foobar/
            ("foo", false, "foobar", Skip),      // object: prefix of w but not a dir
            ("foo", false, "foo", Skip),         // object == w
            ("d/", true, "d/", DescendOnly),     // dir == w: children > w remain
            ("", true, "anything", DescendOnly), // root is ancestor of everything
            ("b/", true, "a/z", Process),
            ("b", false, "a/z", Process),
        ];
        for (rel, is_dir, w, want) in cases {
            assert_eq!(
                resume_action(rel, *is_dir, w),
                *want,
                "resume_action({rel:?}, {is_dir}, {w:?})"
            );
        }
    }

    #[test]
    fn resume_list_token_cases() {
        let cases: &[(&str, &str, Option<&str>)] = &[
            ("a/", "a/m/x", Some("a/m")), // w inside child a/m/ ⇒ keep that prefix listed
            ("a/", "a/m", Some("a/m")),   // w is a direct object
            ("", "m/x", Some("m")),
            ("a/", "b/x", None), // w outside dir
            ("a/", "a/", None),  // w == dir
            ("a//", "a//b/c", Some("a//b")),
            ("a/", "a//b", Some("a/")), // empty segment child a// ⇒ token "a/"
        ];
        for (dir, w, want) in cases {
            assert_eq!(
                resume_list_token(dir, w).as_deref(),
                *want,
                "resume_list_token({dir:?}, {w:?})"
            );
        }
    }

    #[test]
    fn cursor_round_trip_and_rejections() {
        let scope = cursor_scope("srcb", "s/p/", "dstb", "d/");
        let c = CursorV1::new(&scope, "builds/v2/app.zip");
        let json = c.to_json();
        assert_eq!(load_cursor(Some(&json), &scope), Some(c.clone()));

        // Legacy flat S3 token (non-JSON) → fresh.
        assert_eq!(load_cursor(Some("builds/v1/old-token"), &scope), None);
        // Scope mismatch (rule re-pointed) → fresh.
        assert_eq!(load_cursor(Some(&json), "other|s/|b|d/"), None);
        // Version bump → fresh.
        let v2 = json.replace("\"v\":1", "\"v\":2");
        assert_eq!(load_cursor(Some(&v2), &scope), None);
        // Empty pos → fresh.
        let empty = CursorV1::new(&scope, "").to_json();
        assert_eq!(load_cursor(Some(&empty), &scope), None);
        assert_eq!(load_cursor(None, &scope), None);
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn seg() -> impl Strategy<Value = String> {
        prop_oneof![
            Just(String::new()), // empty segment ⇒ "//" in paths
            Just("a".to_string()),
            Just("b".to_string()),
            Just("a-b".to_string()),
            Just("foo".to_string()),
            Just(".deltaglider".to_string()),
            Just(".well-known".to_string()),
            Just("é⽇".to_string()),
            Just("a b".to_string()),
        ]
    }

    /// Relative object path: segments joined by '/', non-empty overall.
    fn rel_path() -> impl Strategy<Value = String> {
        proptest::collection::vec(seg(), 1..5).prop_map(|v| v.join("/"))
    }

    /// Relative dir path: like rel_path but '/'-terminated (root excluded).
    fn rel_dir() -> impl Strategy<Value = String> {
        proptest::collection::vec(seg(), 1..4).prop_map(|v| format!("{}/", v.join("/")))
    }

    proptest! {
        /// P9a — sibling-subtree bound (the ordering lemma the whole walk
        /// rests on): for sibling child prefixes p < p', EVERY descendant
        /// of p sorts before p'.
        #[test]
        fn sibling_subtree_bound(d in prop_oneof![Just(String::new()), rel_dir()],
                                 s1 in seg(), s2 in seg(),
                                 y in rel_path()) {
            prop_assume!(s1 != s2);
            let p1 = format!("{d}{s1}/");
            let p2 = format!("{d}{s2}/");
            let (lo, hi) = if p1 < p2 { (p1, p2) } else { (p2, p1) };
            let descendant = format!("{lo}{y}");
            prop_assert!(descendant < hi,
                "descendant {descendant:?} of {lo:?} must sort before sibling {hi:?}");
        }

        /// P9b — resume_action soundness against brute-force descendants:
        /// Skip ⇒ every possible descendant ≤ w; Process ⇒ none is.
        #[test]
        fn resume_action_sound(d in rel_dir(), w in rel_path(), y in rel_path()) {
            let descendant = format!("{d}{y}");
            match resume_action(&d, true, &w) {
                ResumeAction::Skip => prop_assert!(descendant < w,
                    "Skip({d:?}) but descendant {descendant:?} >= watermark {w:?}"),
                ResumeAction::Process => prop_assert!(descendant > w,
                    "Process({d:?}) but descendant {descendant:?} <= watermark {w:?}"),
                ResumeAction::DescendOnly => {
                    // Ancestor of w: both settled (< w) and pending (> w)
                    // descendants can exist — nothing to assert here beyond
                    // the ancestor relation itself.
                    prop_assert!(w.starts_with(&d));
                }
            }
        }

        /// P10 — resume_list_token drops only settled entries, under BOTH
        /// backend token dialects (strictly-greater and AWS start_after
        /// with prefix re-emission).
        #[test]
        fn resume_token_drops_only_settled(
            dir in prop_oneof![Just(String::new()), rel_dir()],
            tail in rel_path(),
            names in proptest::collection::btree_set(seg(), 1..8),
            dirs_not_objs in proptest::collection::vec(any::<bool>(), 8),
        ) {
            let w = format!("{dir}{tail}");
            let Some(token) = resume_list_token(&dir, &w) else {
                return Ok(()); // w outside dir or == dir: fresh listing, nothing dropped
            };

            for (i, name) in names.iter().enumerate() {
                let is_dir = dirs_not_objs[i % dirs_not_objs.len()];
                let full = if is_dir {
                    format!("{dir}{name}/")
                } else {
                    format!("{dir}{name}")
                };
                let action = resume_action(&full, is_dir, &w);

                // Dialect A: strictly-greater (engine `paginate_sorted`).
                let emitted_strict = full.as_str() > token.as_str();
                // Dialect B: AWS start_after — same order filter on keys, but a
                // common prefix re-emits when ANY key under it survives; the
                // prefix containing the token itself therefore re-emits.
                let emitted_aws = emitted_strict
                    || (is_dir && token.starts_with(full.as_str()));

                for (emitted, dialect) in [(emitted_strict, "strict"), (emitted_aws, "aws")] {
                    if !emitted {
                        prop_assert_eq!(action, ResumeAction::Skip,
                            "{} dialect dropped {:?} (token {:?}, w {:?}) but action is {:?}",
                            dialect, &full, &token, &w, action);
                    }
                }
            }
        }

        /// P8 — ContiguousTracker vs brute-force model under randomized
        /// out-of-order settles with a moving listing marker.
        #[test]
        fn tracker_matches_bruteforce_model(
            raw_paths in proptest::collection::btree_set(rel_path(), 1..20),
            order_seed in proptest::collection::vec(any::<u16>(), 1..64),
            concurrency in 1usize..5,
        ) {
            let paths: Vec<String> = raw_paths.into_iter().collect(); // sorted, distinct
            let mut t = ContiguousTracker::new();

            // Model state: items and the single stream marker tracked apart.
            let mut items_open: Vec<String> = Vec::new();
            let mut settled_model: BTreeSet<String> = BTreeSet::new();

            let mut next_to_open = 0usize;
            let mut seed_idx = 0usize;
            let mut take = |n: usize| {
                let v = order_seed[seed_idx % order_seed.len()] as usize % n.max(1);
                seed_idx += 1;
                v
            };

            // Marker trails the opens, like a listing frontier. It stays
            // alive until the final drain, which breaks out immediately.
            let mut marker_pos = "!".to_string(); // '!' < every generated path
            t.open(&marker_pos);

            loop {
                let can_open =
                    next_to_open < paths.len() && items_open.len() < concurrency;

                if can_open && (items_open.is_empty() || take(2) == 0) {
                    // Open next path in lex order; marker advances with it
                    // (a listing emits, then the frontier moves).
                    let p = paths[next_to_open].clone();
                    next_to_open += 1;
                    t.open(&p);
                    items_open.push(p.clone());
                    if p > marker_pos {
                        t.advance_marker(&marker_pos, &p);
                        marker_pos = p;
                    }
                } else if !items_open.is_empty() {
                    // Settle a random open item (out-of-order completion).
                    let victim = items_open.remove(take(items_open.len()));
                    t.settle(&victim);
                    settled_model.insert(victim);
                } else {
                    // No items, nothing left to open: close the marker, done.
                    t.close(&marker_pos);
                    break;
                }

                // Invariant: watermark == max settled path strictly below the
                // smallest open entry (items + marker). Opens are monotone
                // (sorted order), so this model value is monotone too.
                let min_open = items_open.iter().chain(Some(&marker_pos)).min();
                let expect = settled_model
                    .iter()
                    .rfind(|s| min_open.is_none_or(|o| s.as_str() < o.as_str()));
                prop_assert_eq!(t.watermark(), expect.map(|s| s.as_str()),
                    "items {:?} marker {:?}", items_open, marker_pos);
            }

            // Fully drained: watermark must equal the maximum settled path.
            prop_assert!(t.is_empty());
            prop_assert_eq!(
                t.watermark(),
                settled_model.iter().next_back().map(|s| s.as_str())
            );
        }
    }
}
