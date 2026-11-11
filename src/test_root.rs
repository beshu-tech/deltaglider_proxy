// SPDX-License-Identifier: BUSL-1.1

//! Unit-test twin of `tests/common/server.rs::root_skip` (E1 / F26).
//!
//! A fault test that injects its fault with mode bits proves nothing as root:
//! root ignores them. It calls [`root_skip`] and returns. The skip is loud,
//! and a run that sets `DGP_TEST_REQUIRE_NONROOT` (the non-root CI job, a
//! developer run) fails instead of passing without a check.

/// Does an env value switch the flag on? Unset, empty, `0` and `false` are off.
fn flag_on(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// Is this process root? Root ignores mode bits.
pub(crate) fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Report a root skip. Panics when `DGP_TEST_REQUIRE_NONROOT` is on.
pub(crate) fn root_skip(test: &str) {
    root_skip_with(
        test,
        std::env::var("DGP_TEST_REQUIRE_NONROOT").ok().as_deref(),
    );
}

fn root_skip_with(test: &str, require_nonroot: Option<&str>) {
    if flag_on(require_nonroot) {
        panic!("{test} needs a non-root user (DGP_TEST_REQUIRE_NONROOT is set): root ignores mode bits");
    }
    eprintln!("SKIPPED AS ROOT: {test} injects its fault with mode bits and checks nothing here");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_truth_table() {
        for on in ["1", "true", "yes", " 1 "] {
            assert!(flag_on(Some(on)), "{on:?}");
        }
        for off in ["", "0", "false", "FALSE", "  "] {
            assert!(!flag_on(Some(off)), "{off:?}");
        }
        assert!(!flag_on(None));
    }

    #[test]
    #[should_panic(expected = "needs a non-root user")]
    fn a_required_nonroot_run_fails_a_root_skip() {
        root_skip_with("x", Some("1"));
    }

    #[test]
    fn an_optional_nonroot_run_only_reports_a_root_skip() {
        root_skip_with("x", None);
    }
}
