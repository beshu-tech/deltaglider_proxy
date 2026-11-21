// SPDX-License-Identifier: BUSL-1.1

//! THE wildcard matcher for text that a client controls: `*` matches any
//! run of characters, `?` matches exactly one character, anything else
//! matches itself. Used by the IAM policy evaluator (resource globs and the
//! `StringLike` family of conditions) and by the OIDC mapping rules.
//!
//! The matcher remembers the last `*` and, on a mismatch, lets it swallow one
//! more character. It never recurses and never allocates, so a request cannot
//! make one match cost more than `pattern.len() * text.len()` steps, and a
//! fixed pattern costs a fixed number of steps for each character of text.
//! (`iam-rs` matches with a recursive search that copies both strings on every
//! call: the cost grows with the text to the power of the number of `*`.)

#[cfg(test)]
thread_local! {
    /// The loop steps of the matches that ran on this thread (tests only).
    pub(crate) static STEPS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Bytes in the UTF-8 character that starts with `lead`.
fn char_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Does `text` match `pattern`, where `*` and `?` are wildcards? See the
/// module docs.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    glob(pattern, text, true)
}

/// [`wildcard_match`] where only `*` is a wildcard and `?` matches itself
/// (the email patterns of the OIDC mapping rules).
pub fn star_match(pattern: &str, text: &str) -> bool {
    glob(pattern, text, false)
}

fn glob(pattern: &str, text: &str, question_is_wild: bool) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    // Where the match resumes after the most recent `*`.
    let mut star: Option<(usize, usize)> = None;
    let (mut pi, mut ti) = (0, 0);
    while ti < text.len() {
        #[cfg(test)]
        STEPS.with(|s| s.set(s.get() + 1));
        match pattern.get(pi) {
            Some(b'*') => {
                star = Some((pi + 1, ti));
                pi += 1;
            }
            Some(b'?') if question_is_wild => {
                pi += 1;
                ti += char_len(text[ti]);
            }
            Some(&c) if c == text[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((resume_pi, star_ti)) => {
                    // The last `*` swallows one more character.
                    let next = star_ti + char_len(text[star_ti]);
                    star = Some((resume_pi, next));
                    pi = resume_pi;
                    ti = next;
                }
                None => return false,
            },
        }
    }
    pattern
        .get(pi..)
        .unwrap_or_default()
        .iter()
        .all(|&c| c == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn steps_of(pattern: &str, text: &str) -> (bool, u64) {
        let before = STEPS.with(std::cell::Cell::get);
        let matched = wildcard_match(pattern, text);
        (matched, STEPS.with(std::cell::Cell::get) - before)
    }

    #[test]
    fn matches_like_a_shell_glob() {
        assert!(wildcard_match("hello", "hello"));
        assert!(wildcard_match("h*", "hello"));
        assert!(wildcard_match("*llo", "hello"));
        assert!(wildcard_match("h*o", "hello"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("**", "abc"));
        assert!(wildcard_match("h?llo", "hello"));
        assert!(wildcard_match("?", "é"), "? is one character, not one byte");
        assert!(!wildcard_match("??", "é"));
        assert!(wildcard_match("*@acme.com", "dana@acme.com"));
        assert!(!wildcard_match("h*x", "hello"));
        assert!(!wildcard_match("hello", "hell"));
        assert!(!wildcard_match("hell", "hello"));
        assert!(!wildcard_match("", "x"));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("?", ""));
    }

    #[test]
    fn star_match_takes_a_question_mark_literally() {
        assert!(star_match("a?b", "a?b"));
        assert!(!star_match("a?b", "axb"));
        assert!(star_match("*@acme.com", "dana?x@acme.com"));
    }

    /// The reference: the recursive search of iam-rs.
    fn reference(pattern: &str, text: &str) -> bool {
        iam_rs::Arn::wildcard_match(text, pattern)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// Same verdict as iam-rs, on a small alphabet that forces backtracking.
        #[test]
        fn agrees_with_iam_rs(
            pattern in "[ab*?é/]{0,10}",
            text in "[abé/]{0,12}",
        ) {
            prop_assert_eq!(wildcard_match(&pattern, &text), reference(&pattern, &text));
        }
    }

    /// The cost is bounded by `pattern * text` steps, also for the pattern
    /// that makes a recursive matcher try every split of the text.
    #[test]
    fn the_steps_never_exceed_the_pattern_times_the_text() {
        for stars in [1usize, 2, 4, 8, 16] {
            let pattern = format!("{}b", "*a".repeat(stars));
            for len in [16usize, 256, 1024] {
                let text = "a".repeat(len);
                let (matched, steps) = steps_of(&pattern, &text);
                assert!(!matched);
                assert!(
                    steps <= (pattern.len() * len) as u64,
                    "{stars} stars, {len} chars: {steps} steps"
                );
            }
        }
    }

    /// For a fixed pattern the steps grow in proportion to the text: ten
    /// times the text costs less than eleven times the steps, however many
    /// `*` the pattern has.
    #[test]
    fn the_steps_grow_in_proportion_to_the_text() {
        for stars in [1usize, 3, 6] {
            let pattern = format!("{}b", "*a".repeat(stars));
            let small = steps_of(&pattern, &"a".repeat(100)).1;
            let large = steps_of(&pattern, &"a".repeat(1000)).1;
            assert!(
                large <= small * 11,
                "{stars} stars: {small} steps for 100 chars, {large} for 1000"
            );
        }
    }
}
