// SPDX-License-Identifier: BUSL-1.1

//! Chaos hooks for the integration tests (`DGP_TEST_*`).
//!
//! The integration tests set these variables on the proxy they spawn, to
//! hold a window open or to fail a step on purpose. They are read once per
//! process and only in debug builds, which is what `cargo test` builds: a
//! release binary compiles every seam to "off" and ignores the variables.

use super::{lookup_bool, lookup_parse, EnvLookup};
use std::collections::BTreeSet;
use std::sync::LazyLock;

/// Every test seam, parsed. The default is "all off".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSeams {
    /// `DGP_TEST_COMPLETE_STALL_MS`: sleep before a CompleteMultipartUpload
    /// stores, so a test can race the store window.
    pub complete_stall_ms: u64,
    /// `DGP_TEST_FAIL_PART_ONCE`: the part number of a streaming copy that
    /// fails once with a transient error.
    pub fail_part_once: Option<i32>,
    /// `DGP_TEST_PART_BARRIER` (+ `DGP_TEST_PART_DELAY_MS`, default 150):
    /// hold each copy part so the parts are co-resident.
    pub part_delay_ms: Option<u64>,
    /// `DGP_TEST_OBJECT_BARRIER` (+ `DGP_TEST_OBJECT_DELAY_MS`, default 150):
    /// hold each replicated object so the objects are co-resident.
    pub object_delay_ms: Option<u64>,
    /// `DGP_TEST_COPY_STALL_MS`: stall per replicated / deleted object.
    pub copy_stall_ms: u64,
    /// `DGP_TEST_MAX_JOB_PAGES`: a smaller page budget for job loops.
    pub max_job_pages: Option<u32>,
    /// `DGP_TEST_FORCE_NONCAS_BACKEND` (comma-separated): backends that get
    /// a forced NonCas capability verdict without a probe.
    pub force_noncas_backends: BTreeSet<String>,
}

impl TestSeams {
    /// Parse every seam from `env`.
    pub fn from_env(env: EnvLookup) -> Self {
        let delay = |flag: &str, ms: &str| {
            lookup_bool(env, flag, false).then(|| lookup_parse(env, ms).unwrap_or(150))
        };
        Self {
            complete_stall_ms: lookup_parse(env, "DGP_TEST_COMPLETE_STALL_MS").unwrap_or(0),
            fail_part_once: lookup_parse::<i32>(env, "DGP_TEST_FAIL_PART_ONCE").filter(|n| *n >= 0),
            part_delay_ms: delay("DGP_TEST_PART_BARRIER", "DGP_TEST_PART_DELAY_MS"),
            object_delay_ms: delay("DGP_TEST_OBJECT_BARRIER", "DGP_TEST_OBJECT_DELAY_MS"),
            copy_stall_ms: lookup_parse(env, "DGP_TEST_COPY_STALL_MS").unwrap_or(0),
            max_job_pages: lookup_parse::<u32>(env, "DGP_TEST_MAX_JOB_PAGES").filter(|n| *n > 0),
            force_noncas_backends: env("DGP_TEST_FORCE_NONCAS_BACKEND")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// The seams of this process: parsed from the environment on first use in a
/// debug build, all off in a release build.
pub fn test_seams() -> &'static TestSeams {
    static SEAMS: LazyLock<TestSeams> = LazyLock::new(|| {
        if cfg!(debug_assertions) {
            TestSeams::from_env(&super::process_env)
        } else {
            TestSeams::default()
        }
    });
    &SEAMS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &'static [(&'static str, &'static str)]) -> TestSeams {
        TestSeams::from_env(&move |n: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn unset_means_off() {
        assert_eq!(from(&[]), TestSeams::default());
    }

    #[test]
    fn every_seam_is_read() {
        let t = from(&[
            ("DGP_TEST_COMPLETE_STALL_MS", "1500"),
            ("DGP_TEST_FAIL_PART_ONCE", "2"),
            ("DGP_TEST_PART_BARRIER", "1"),
            ("DGP_TEST_OBJECT_BARRIER", "true"),
            ("DGP_TEST_OBJECT_DELAY_MS", "20"),
            ("DGP_TEST_COPY_STALL_MS", "50"),
            ("DGP_TEST_MAX_JOB_PAGES", "3"),
            ("DGP_TEST_FORCE_NONCAS_BACKEND", " b2sim, ,other "),
        ]);
        assert_eq!(t.complete_stall_ms, 1500);
        assert_eq!(t.fail_part_once, Some(2));
        assert_eq!(t.part_delay_ms, Some(150), "the default delay");
        assert_eq!(t.object_delay_ms, Some(20));
        assert_eq!(t.copy_stall_ms, 50);
        assert_eq!(t.max_job_pages, Some(3));
        assert_eq!(
            t.force_noncas_backends,
            ["b2sim", "other"].map(String::from).into()
        );
    }

    #[test]
    fn inert_values_stay_off() {
        let t = from(&[
            ("DGP_TEST_FAIL_PART_ONCE", "-1"),
            ("DGP_TEST_PART_BARRIER", "0"),
            ("DGP_TEST_PART_DELAY_MS", "500"),
            ("DGP_TEST_MAX_JOB_PAGES", "0"),
        ]);
        assert_eq!(t, TestSeams::default());
    }
}
