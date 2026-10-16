// SPDX-License-Identifier: BUSL-1.1

//! THE include/exclude glob filter of replication rules, lifecycle rules and
//! Slack notifications: one compiler and one verdict, so the rule, its
//! preview and its delete paths agree on every key.
//!
//! Semantics (the `globset` defaults):
//! - A glob matches the WHOLE key from the bucket root. A rule's prefix is
//!   part of the key: with prefix `ror/`, `builds/**` matches nothing and
//!   `ror/builds/**` matches the builds.
//! - `*` matches any run of characters, `/` included: `*.tmp` matches
//!   `logs/a.tmp`, and `logs/*` matches every key under `logs/`.
//! - `**` matches any depth, `?` one character, `[ab]` a class, `{a,b}`
//!   either form.
//! - An empty include list includes every key. An exclude wins over an
//!   include.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// A glob that does not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidGlob {
    pub pattern: String,
    pub reason: String,
}

impl std::fmt::Display for InvalidGlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid glob {:?}: {}", self.pattern, self.reason)
    }
}

/// Compile one list of globs; the first glob that does not compile is the
/// error. An empty list compiles to an empty set.
pub fn compile(patterns: &[String]) -> Result<GlobSet, InvalidGlob> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| InvalidGlob {
            pattern: pattern.clone(),
            reason: e.to_string(),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| InvalidGlob {
        pattern: patterns.join(", "),
        reason: e.to_string(),
    })
}

/// What the filters say about one key. The index names the glob that
/// decided: its position in the include or the exclude list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// In scope: no include list (`None`), or this include matched.
    Included(Option<usize>),
    /// This exclude matched (it wins over any include).
    Excluded(usize),
    /// There is an include list and no include matched.
    NotIncluded,
}

/// The verdict of `include`/`exclude` on `key`.
pub fn verdict(key: &str, include: &GlobSet, exclude: &GlobSet) -> Verdict {
    if let Some(i) = first_match(exclude, key) {
        return Verdict::Excluded(i);
    }
    if include.is_empty() {
        return Verdict::Included(None);
    }
    match first_match(include, key) {
        Some(i) => Verdict::Included(Some(i)),
        None => Verdict::NotIncluded,
    }
}

/// Whether the filters keep `key` in scope.
pub fn in_scope(key: &str, include: &GlobSet, exclude: &GlobSet) -> bool {
    matches!(verdict(key, include, exclude), Verdict::Included(_))
}

fn first_match(set: &GlobSet, key: &str) -> Option<usize> {
    set.matches(key).into_iter().min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(globs: &[&str]) -> GlobSet {
        compile(&globs.iter().map(|g| g.to_string()).collect::<Vec<_>>()).unwrap()
    }

    /// The semantics the docs and the preview promise.
    #[test]
    fn the_glob_semantics() {
        let none = set(&[]);
        for (glob, key, hit) in [
            // `*` crosses `/`.
            ("*.tmp", "logs/a.tmp", true),
            ("logs/*", "logs/x/y.zip", true),
            // The whole key, from the bucket root.
            ("builds/**", "ror/builds/a.zip", false),
            ("ror/builds/**", "ror/builds/a.zip", true),
            ("**/reports/**", "ror/e2e/reports/x.png", true),
            ("ror/*.zip", "ror/builds/a.zip", true),
            ("?.zip", "a.zip", true),
            ("[ab].zip", "c.zip", false),
            ("*.{zip,tar}", "x/y.tar", true),
        ] {
            assert_eq!(in_scope(key, &set(&[glob]), &none), hit, "{glob} vs {key}");
        }
    }

    #[test]
    fn an_exclude_wins_and_the_deciding_glob_is_named() {
        let include = set(&["ror/**", "*.zip"]);
        let exclude = set(&["*.tmp", "ror/tmp/**"]);
        assert_eq!(
            verdict("ror/tmp/a.tmp", &include, &exclude),
            Verdict::Excluded(0)
        );
        assert_eq!(
            verdict("ror/a.zip", &include, &exclude),
            Verdict::Included(Some(0))
        );
        assert_eq!(
            verdict("x/a.zip", &include, &exclude),
            Verdict::Included(Some(1))
        );
        assert_eq!(verdict("x/a.txt", &include, &exclude), Verdict::NotIncluded);
        assert_eq!(
            verdict("x/a.txt", &set(&[]), &set(&[])),
            Verdict::Included(None)
        );
    }

    #[test]
    fn a_bad_glob_is_named() {
        let err = compile(&["ok/**".into(), "bad[".into()]).unwrap_err();
        assert_eq!(err.pattern, "bad[");
    }
}
