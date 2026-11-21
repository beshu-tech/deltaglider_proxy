// SPDX-License-Identifier: BUSL-1.1

//! What a `path_glob` can say about the keys BELOW a prefix.
//!
//! A LIST names a prefix, not a key. To admit it, or to hide a folder name,
//! the rule needs two facts about the glob: can SOME key that starts with the
//! prefix match, and do ALL of them. Both come from the DFA of the regex that
//! globset itself matches with, so they agree with `GlobMatcher::is_match`.
//! When the DFA cannot be built the answers are the safe ones: "some key may
//! match" (the per-key filter still decides) and "not all".

use regex_automata::dfa::{dense, Automaton};
use regex_automata::util::{primitives::StateID, syntax};
use regex_automata::{nfa::thompson, Anchored, Input};
use std::collections::HashMap;

/// States of the DFA that the analysis walks at most; a bigger glob is "unknown".
const MAX_STATES: usize = 4096;

/// The reach of one compiled glob.
#[derive(Debug)]
pub struct GlobReach {
    analysis: Option<Analysis>,
}

#[derive(Debug)]
struct Analysis {
    /// Kept for the request: `at` walks the bytes of a prefix through its
    /// transitions. Its size is bounded by the build limits below.
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
    /// The flags of each reached state, indexed by [`Analysis::slot`]; the
    /// build tables (successors, reverse edges) are dropped after the flags.
    flags: Vec<Option<Flags>>,
}

#[derive(Debug, Clone, Copy)]
struct Flags {
    /// Some continuation of the text read so far matches.
    can: bool,
    /// Every continuation matches.
    all: bool,
    /// Some continuation with no further `/` matches: a key directly in the
    /// folder that the text read so far names.
    flat: bool,
}

impl GlobReach {
    pub fn new(glob: &globset::Glob) -> Self {
        Self {
            analysis: Analysis::build(glob.regex()),
        }
    }

    /// A reach that knows nothing: every prefix may lead to a match, none
    /// is covered entirely.
    #[cfg(test)]
    fn unknown() -> Self {
        Self { analysis: None }
    }

    /// Can a key that starts with `prefix` match the glob?
    pub fn can_match_below(&self, prefix: &str) -> bool {
        self.analysis
            .as_ref()
            .is_none_or(|a| a.at(prefix).is_none_or(|f| f.can))
    }

    /// Can a key DIRECTLY in `folder` (`a/b/`, or `""` for the root: the
    /// folder text, then a name with no `/`) match the glob?
    pub fn can_match_in_folder(&self, folder: &str) -> bool {
        self.analysis
            .as_ref()
            .is_none_or(|a| a.at(folder).is_none_or(|f| f.flat))
    }

    /// Does every key that starts with `prefix` match the glob?
    pub fn matches_all_below(&self, prefix: &str) -> bool {
        self.analysis
            .as_ref()
            .is_some_and(|a| a.at(prefix).is_some_and(|f| f.all))
    }
}

impl Analysis {
    fn build(regex: &str) -> Option<Self> {
        Self::build_counted(regex).0
    }

    /// [`Self::build`], and the steps of the analysis (each successor read and
    /// each reverse edge walked), for the cost test.
    fn build_counted(regex: &str) -> (Option<Self>, u64) {
        let mut steps: u64 = 0;
        (Self::build_with(regex, &mut steps), steps)
    }

    /// The table slot of a state of `dfa`: its id divided by the stride.
    fn slot_of(dfa: &dense::DFA<Vec<u32>>, s: StateID) -> usize {
        s.as_usize() >> dfa.stride2()
    }

    fn slot(&self, s: StateID) -> usize {
        Self::slot_of(&self.dfa, s)
    }

    fn build_with(regex: &str, steps: &mut u64) -> Option<Self> {
        let dfa = dense::Builder::new()
            .configure(
                dense::Config::new()
                    .start_kind(regex_automata::dfa::StartKind::Anchored)
                    .dfa_size_limit(Some(4 << 20))
                    .determinize_size_limit(Some(4 << 20)),
            )
            .syntax(syntax::Config::new().utf8(false).dot_matches_new_line(true))
            .thompson(thompson::Config::new().utf8(false))
            .build(regex)
            .ok()?;
        let start = dfa
            .start_state_forward(&Input::new("").anchored(Anchored::Yes))
            .ok()?;
        // Every state reachable from the start, numbered in discovery order;
        // `preds[t]` lists the states with an edge to `t` (each once).
        let mut order: Vec<StateID> = vec![start];
        let mut index: HashMap<StateID, usize> = HashMap::from([(start, 0)]);
        let mut preds: Vec<Vec<usize>> = vec![Vec::new()];
        // The same edges without the `/` byte: a name inside one folder.
        let mut flat_preds: Vec<Vec<usize>> = vec![Vec::new()];
        let mut i = 0;
        while i < order.len() {
            let s = order[i];
            let mut seen_here: Vec<usize> = Vec::new();
            let mut flat_seen_here: Vec<usize> = Vec::new();
            for b in 0..=255u8 {
                *steps += 1;
                let t = dfa.next_state(s, b);
                let ti = match index.get(&t) {
                    Some(&ti) => ti,
                    None => {
                        let ti = order.len();
                        index.insert(t, ti);
                        order.push(t);
                        preds.push(Vec::new());
                        flat_preds.push(Vec::new());
                        if order.len() > MAX_STATES {
                            return None;
                        }
                        ti
                    }
                };
                if !seen_here.contains(&ti) {
                    seen_here.push(ti);
                    preds[ti].push(i);
                }
                if b != b'/' && !flat_seen_here.contains(&ti) {
                    flat_seen_here.push(ti);
                    flat_preds[ti].push(i);
                }
            }
            i += 1;
        }
        let accepts: Vec<bool> = order
            .iter()
            .map(|&s| dfa.is_match_state(dfa.next_eoi_state(s)))
            .collect();
        // `can` is the least fixpoint: a state can match when it accepts or
        // some successor can. Walk the reverse edges from the accepting states.
        let mut can = accepts.clone();
        let mut work: Vec<usize> = (0..order.len()).filter(|&s| can[s]).collect();
        while let Some(t) = work.pop() {
            for &p in &preds[t] {
                *steps += 1;
                if !can[p] {
                    can[p] = true;
                    work.push(p);
                }
            }
        }
        // `flat` is `can` over the edges that are not `/`.
        let mut flat = accepts.clone();
        let mut work: Vec<usize> = (0..order.len()).filter(|&s| flat[s]).collect();
        while let Some(t) = work.pop() {
            for &p in &flat_preds[t] {
                *steps += 1;
                if !flat[p] {
                    flat[p] = true;
                    work.push(p);
                }
            }
        }
        // `all` is the greatest fixpoint: every continuation matches when the
        // state accepts and every successor does. Walk the reverse edges from
        // the states that do not accept.
        let mut all = accepts;
        let mut work: Vec<usize> = (0..order.len()).filter(|&s| !all[s]).collect();
        while let Some(t) = work.pop() {
            for &p in &preds[t] {
                *steps += 1;
                if all[p] {
                    all[p] = false;
                    work.push(p);
                }
            }
        }
        let slots = order
            .iter()
            .map(|&s| Self::slot_of(&dfa, s))
            .max()
            .map_or(0, |m| m + 1);
        let mut flags: Vec<Option<Flags>> = vec![None; slots];
        for (i, &s) in order.iter().enumerate() {
            flags[Self::slot_of(&dfa, s)] = Some(Flags {
                can: can[i],
                all: all[i],
                flat: flat[i],
            });
        }
        Some(Self { dfa, start, flags })
    }

    /// The flags after reading `prefix`, `None` when the DFA has no state
    /// for it (it cannot happen: every transition is walked).
    fn at(&self, prefix: &str) -> Option<Flags> {
        let mut s = self.start;
        for b in prefix.bytes() {
            s = self.dfa.next_state(s, b);
        }
        self.flags.get(self.slot(s)).copied().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reach(glob: &str) -> (GlobReach, globset::GlobMatcher) {
        let g = globset::Glob::new(glob).unwrap();
        (GlobReach::new(&g), g.compile_matcher())
    }

    #[test]
    fn a_folder_glob_covers_everything_below_its_folder() {
        let (r, _) = reach("secret/**");
        assert!(r.matches_all_below("secret/"));
        assert!(r.matches_all_below("secret/a/b"));
        assert!(
            !r.matches_all_below("secret"),
            "`secret` itself is no match"
        );
        assert!(!r.matches_all_below("sec"));
        assert!(!r.matches_all_below(""));
        assert!(r.can_match_below(""));
        assert!(r.can_match_below("sec"));
        assert!(r.can_match_below("secret"));
        assert!(r.can_match_below("secret/x"));
        assert!(!r.can_match_below("public/"));
        assert!(!r.can_match_below("secrets"));
    }

    #[test]
    fn a_suffix_glob_can_match_below_any_prefix_and_covers_none() {
        let (r, _) = reach("*.zip");
        for p in ["", "app.zip", "a/b/", "x"] {
            assert!(r.can_match_below(p), "{p}");
        }
        assert!(!r.matches_all_below(""));
        assert!(
            !r.matches_all_below("app.zip"),
            "`app.zip.bak` does not match"
        );
    }

    #[test]
    fn a_literal_glob_matches_one_key() {
        let (r, _) = reach("builds/");
        assert!(r.can_match_below("builds/"));
        assert!(r.can_match_below("bui"));
        assert!(!r.can_match_below("builds/app.zip"));
        assert!(!r.matches_all_below("builds/"));
    }

    #[test]
    fn alternates_and_classes_are_read_as_globset_reads_them() {
        let (r, _) = reach("{a,b}/**");
        assert!(r.matches_all_below("a/"));
        assert!(r.matches_all_below("b/x"));
        assert!(!r.can_match_below("c/"));
        let (r, _) = reach("[ab]*");
        assert!(r.matches_all_below("a"));
        assert!(!r.matches_all_below("c"));
        assert!(!r.can_match_below("c"));
    }

    /// The analysis costs a number of steps linear in the states and their
    /// edges, counted and never timed: a long literal glob is a chain of
    /// states, which a pass-until-stable loop walked once per link.
    #[test]
    fn the_analysis_is_linear_in_states_times_edges() {
        for len in [50usize, 200, 400] {
            let glob = globset::Glob::new(&"a".repeat(len)).unwrap();
            let (analysis, steps) = Analysis::build_counted(glob.regex());
            let states = analysis
                .expect("a literal glob builds")
                .flags
                .iter()
                .flatten()
                .count() as u64;
            assert!(states >= len as u64, "{states} states for {len} chars");
            assert!(
                steps <= 4 * 256 * states,
                "{len} chars: {steps} steps for {states} states"
            );
        }
    }

    /// Can a key DIRECTLY in a folder (no further `/`) match? The facts of an
    /// object depend only on the keys of its own folder.
    #[test]
    fn a_folder_glob_reaches_the_keys_directly_in_a_folder() {
        let cases = [
            ("secret/hidden*", "secret/", true),
            ("secret/hidden*", "", false),
            ("secret/hidden*", "team/", false),
            ("secret/hidden*", "secret/sub/", false),
            ("secret/**", "secret/", true),
            ("secret/**", "secret/sub/", true),
            ("secret/**", "", false),
            ("*.key", "", true),
            ("*.key", "a/b/", true),
            ("docs/readme.md", "docs/", true),
            ("docs/readme.md", "", false),
            ("**", "x/", true),
        ];
        for (glob, folder, want) in cases {
            let (r, _) = reach(glob);
            assert_eq!(r.can_match_in_folder(folder), want, "{glob} in {folder:?}");
        }
        assert!(GlobReach::unknown().can_match_in_folder("anything/"));
    }

    #[test]
    fn an_unknown_reach_is_safe() {
        let r = GlobReach::unknown();
        assert!(r.can_match_below("anything"));
        assert!(!r.matches_all_below("anything"));
    }

    /// The two answers agree with the matcher on sample keys: a prefix with
    /// `matches_all_below` never has a non-matching extension; a prefix
    /// without `can_match_below` never has a matching one.
    #[test]
    fn the_answers_agree_with_the_matcher() {
        let globs = [
            "secret/**",
            "*.zip",
            "a/**/z",
            "{x,y}*.log",
            "docs/?/*",
            "**",
            "*",
            "a[0-9]b",
        ];
        let alphabet = [
            "", "a", "b", "/", "x", "z", ".zip", ".log", "secret/", "0", "docs/",
        ];
        let mut texts: Vec<String> = vec![String::new()];
        for _ in 0..3 {
            let mut more = Vec::new();
            for t in &texts {
                for a in alphabet {
                    more.push(format!("{t}{a}"));
                }
            }
            more.sort();
            more.dedup();
            texts = more;
            texts.truncate(2500);
        }
        for glob in globs {
            let (r, m) = reach(glob);
            for p in texts.iter().take(300) {
                let ext = texts.iter().filter(|t| t.starts_with(p.as_str()));
                let (mut any, mut all) = (false, true);
                for t in ext {
                    if m.is_match(t) {
                        any = true;
                    } else {
                        all = false;
                    }
                }
                if r.matches_all_below(p) {
                    assert!(all, "{glob} claims all below {p:?}");
                }
                if !r.can_match_below(p) {
                    assert!(!any, "{glob} claims none below {p:?}");
                }
            }
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// One token of a random glob: literals, `*`, `**`, `?`, classes, alternates.
        fn token() -> impl Strategy<Value = &'static str> {
            prop::sample::select(vec![
                "a",
                "b",
                "c",
                "/",
                ".",
                "*",
                "**",
                "**/",
                "/**",
                "?",
                "[ab]",
                "[!a]",
                "{a,b}",
                "{a/,b}",
                "{*.a,b/**}",
                // Escapes, ranges, a multibyte char and long literals.
                "\\*",
                "\\[",
                "\\?",
                "[a-z]",
                "[!a-b]",
                "\u{e9}",
                "\u{1F600}",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc",
            ])
        }

        fn text() -> impl Strategy<Value = String> {
            prop::collection::vec(
                prop::sample::select(vec![
                    'a',
                    'b',
                    'c',
                    '/',
                    '.',
                    '*',
                    '[',
                    '?',
                    '\u{e9}',
                    '\u{1F600}',
                ]),
                0..6,
            )
            .prop_map(|c| c.into_iter().collect())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1500))]

            /// Against globset on random globs and random keys: `matches_all_below(p)`
            /// means globset matches every key below `p`; `can_match_below(p) == false`
            /// means it matches none.
            #[test]
            fn the_answers_agree_with_globset_on_random_globs(
                tokens in prop::collection::vec(token(), 1..6),
                prefix in text(),
                suffixes in prop::collection::vec(text(), 1..40),
            ) {
                let glob_text: String = tokens.concat();
                let Ok(glob) = globset::Glob::new(&glob_text) else {
                    return Ok(());
                };
                let reach = GlobReach::new(&glob);
                let matcher = glob.compile_matcher();
                let keys = std::iter::once(prefix.clone())
                    .chain(suffixes.iter().map(|s| format!("{prefix}{s}")));
                let (mut any, mut all, mut any_flat) = (false, true, false);
                for key in keys {
                    if matcher.is_match(&key) {
                        any = true;
                        // A name with no `/` after the prefix.
                        any_flat |= !key[prefix.len()..].contains('/');
                    } else {
                        all = false;
                    }
                }
                if !reach.can_match_in_folder(&prefix) {
                    prop_assert!(!any_flat, "{glob_text:?} claims no key directly in {prefix:?} matches");
                }
                if reach.matches_all_below(&prefix) {
                    prop_assert!(all, "{glob_text:?} claims every key below {prefix:?} matches");
                }
                if !reach.can_match_below(&prefix) {
                    prop_assert!(!any, "{glob_text:?} claims no key below {prefix:?} matches");
                }
            }
        }
    }
}
