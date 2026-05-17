// SPDX-License-Identifier: GPL-3.0-only

//! Include/exclude glob filter for recursive CLI commands (`cp -r`,
//! `rm -r`, and the deferred `sync` / `migrate`).
//!
//! Semantics for the MVP: `matches(key) == true` iff
//!   (no include patterns specified OR any include pattern matches)
//!   AND no exclude pattern matches.
//!
//! Patterns without `/` are tested against the key's basename
//! (`releases/v1.zip` → `v1.zip`); patterns with `/` are tested
//! against the full key. This is simpler than AWS-CLI's strict-
//! order-of-application semantics — good enough for `cp -r` /
//! `rm -r`; revisit when `sync` lands and needs precise parity.

use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Debug)]
pub struct Filter {
    include: GlobSet,
    exclude: GlobSet,
    has_include: bool,
}

impl Filter {
    /// Compile a filter from raw include / exclude patterns. Empty
    /// slices produce a "match-everything" filter.
    pub fn build(includes: &[String], excludes: &[String]) -> Result<Self, globset::Error> {
        Ok(Self {
            include: build_set(includes)?,
            exclude: build_set(excludes)?,
            has_include: !includes.is_empty(),
        })
    }

    /// Returns true iff this filter would accept `key`. Pure: no I/O,
    /// no side effects.
    pub fn matches(&self, key: &str) -> bool {
        let basename = key.rsplit('/').next().unwrap_or(key);
        // Test full path AND basename so simple `*.zip` works AND
        // path-aware `releases/*.zip` works.
        let include_ok =
            !self.has_include || self.include.is_match(basename) || self.include.is_match(key);
        if !include_ok {
            return false;
        }
        let excluded = self.exclude.is_match(basename) || self.exclude.is_match(key);
        !excluded
    }
}

fn build_set(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p)?);
    }
    b.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(includes: &[&str], excludes: &[&str]) -> Filter {
        let inc: Vec<String> = includes.iter().map(|s| s.to_string()).collect();
        let exc: Vec<String> = excludes.iter().map(|s| s.to_string()).collect();
        Filter::build(&inc, &exc).expect("filter compiles")
    }

    #[test]
    fn empty_filter_matches_everything() {
        let filter = f(&[], &[]);
        assert!(filter.matches("foo.zip"));
        assert!(filter.matches("releases/foo.zip"));
        assert!(filter.matches(""));
    }

    #[test]
    fn include_only_acts_as_allowlist() {
        let filter = f(&["*.zip"], &[]);
        assert!(filter.matches("foo.zip"));
        assert!(filter.matches("releases/foo.zip"));
        assert!(!filter.matches("foo.txt"));
    }

    #[test]
    fn exclude_only_acts_as_denylist() {
        let filter = f(&[], &["*.tmp"]);
        assert!(filter.matches("foo.zip"));
        assert!(!filter.matches("foo.tmp"));
        assert!(!filter.matches("releases/foo.tmp"));
    }

    #[test]
    fn exclude_wins_over_include() {
        let filter = f(&["*.zip"], &["secret.zip"]);
        assert!(filter.matches("foo.zip"));
        assert!(!filter.matches("secret.zip"));
        assert!(!filter.matches("releases/secret.zip"));
    }

    #[test]
    fn path_aware_pattern_matches_full_key() {
        let filter = f(&["releases/*.zip"], &[]);
        assert!(filter.matches("releases/v1.zip"));
        // Basename pattern wouldn't match a key in a different prefix;
        // path-aware pattern is strict.
        assert!(!filter.matches("builds/v1.zip"));
    }

    #[test]
    fn rejects_when_no_include_matches() {
        let filter = f(&["*.zip", "*.tar"], &[]);
        assert!(!filter.matches("foo.txt"));
        assert!(!filter.matches("README"));
    }
}
