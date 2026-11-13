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
    /// `DGP_TEST_MAINTENANCE_OBJECT_STALL_MS`: stall before each object of
    /// a paged maintenance phase (reencrypt, backfill, migrate), so a test
    /// can act while the job is still running.
    pub maintenance_object_stall_ms: u64,
    /// `DGP_TEST_LIFECYCLE_OBJECT_STALL_MS`: stall before each object of a
    /// lifecycle run, so a test can watch its progress and pause or kill
    /// it while it runs.
    pub lifecycle_object_stall_ms: u64,
    /// `DGP_TEST_READ_OTHER_FORM_STALL_MS`: sleep before a plain GET's HEAD
    /// of the other stored form, so a test can switch the form inside the
    /// read.
    pub read_other_form_stall_ms: u64,
    /// `DGP_TEST_MAX_JOB_PAGES`: a smaller page budget for job loops.
    pub max_job_pages: Option<u32>,
    /// `DGP_TEST_FORCE_NONCAS_BACKEND` (comma-separated): backends that get
    /// a forced NonCas capability verdict without a probe.
    pub force_noncas_backends: BTreeSet<String>,
    /// `DGP_TEST_FORCE_NONCAS_WHILE_FILE`: the forced verdict of
    /// `DGP_TEST_FORCE_NONCAS_BACKEND` holds only while this file exists, so
    /// a test can turn a backend's verdict to CAS without a restart (the
    /// next probe, e.g. "Check again", sees it).
    pub force_noncas_while_file: Option<std::path::PathBuf>,
    /// `DGP_TEST_CONVERSION_GRACE_SECS`: the grace after a conversion's
    /// cutoff (default 300 s), so a test can plant an object after it.
    pub conversion_grace_secs: Option<i64>,
    /// `DGP_TEST_CONVERSION_WINDOW_SECS`: the length of a conversion window
    /// (default 7 days), so a test can see a deadline pass.
    pub conversion_window_secs: Option<i64>,
    /// `DGP_TEST_REWRITE_STALL=<point>:<ms>[:<signal file>]`: the
    /// re-encrypt job's in-place rewrite of an object sleeps `ms` at
    /// `point` ([`RewriteStall`]), so a test can write the object in that
    /// window. With a signal file, each stall first appends the object's
    /// name as one line to it: the test waits for that line instead of a
    /// fixed time.
    pub rewrite_stall: Option<RewriteStallSeam>,
    /// `DGP_TEST_KEEP_WRITE_GATE=true`: a re-encrypt job keeps the bucket's
    /// write gate although its rewrites are conditional. Why it exists:
    /// since N2, encryption runs only on backends with atomic writes, where
    /// every rewrite is conditional, so an ENCRYPT job never arms the gate
    /// on its own. Only a decrypt job on a backend without atomic writes
    /// still does. The gate tests (503 to clients, drain, unwind, the
    /// cancel paths) need those gated mechanics with an encrypt job, which
    /// is what they set up; this seam keeps the gate for them.
    pub keep_write_gate: bool,
    /// `DGP_TEST_ATOMICITY_PROBE_DELAY_MS`: every atomicity probe sleeps
    /// this long first, and a filesystem backend gets its verdict from the
    /// (delayed) probe too, so a test sees the window before the first
    /// verdict.
    pub atomicity_probe_delay_ms: u64,
    /// `DGP_TEST_COPY_OBJECT_MAX_BYTES` (5 MiB to 5 GiB): the size above which
    /// an S3 server-side copy is a multipart copy of UploadPartCopy
    /// requests instead of one CopyObject (production: 5 GiB, S3's limit),
    /// so a test of a few MiB runs that path on MinIO.
    pub copy_object_max_bytes: Option<u64>,
    /// `DGP_TEST_COPY_PART_BYTES` (5 MiB, S3's smallest part, to 5 GiB, its
    /// largest): the part size of a multipart server-side copy (production:
    /// 512 MiB).
    pub copy_part_bytes: Option<u64>,
    /// `DGP_TEST_DISK_FREE_BYTES`: the free space of the spool directory at
    /// boot, so a test sees one auto-sized limit on every host. It also
    /// turns off the tmpfs, Kubernetes and shared-device facts
    /// (`limits::measure_disk_facts`).
    pub disk_free_bytes: Option<u64>,
    /// `DGP_TEST_DISK_MONITOR_FREE_PERCENT` (0 to 100): the disk monitor
    /// (`disk_watch`) sees every path as one 100 GiB device with this
    /// percent free, so a test reaches the low, high and critical levels
    /// on any host.
    pub disk_monitor_free_percent: Option<u8>,
}

/// S3's smallest part: the floor of the two copy seams, so a seam never
/// makes a copy that a real S3 would refuse.
pub const COPY_SEAM_FLOOR_BYTES: u64 = 5 * 1024 * 1024;

/// S3's limit of a CopyObject source and of a part: the ceiling of the two
/// copy seams. A value above it is ignored (the seam stays off).
pub const COPY_SEAM_CEILING_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Pure: a copy seam value, if it lies within what a real S3 accepts.
fn copy_seam_bytes(v: Option<u64>) -> Option<u64> {
    v.filter(|n| (COPY_SEAM_FLOOR_BYTES..=COPY_SEAM_CEILING_BYTES).contains(n))
}

/// Where the re-encrypt job's rewrite stalls (`DGP_TEST_REWRITE_STALL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteStall {
    /// `before-lock`: after the job's HEAD of an object, before the rewrite
    /// takes the object's write lock (a write through the same instance
    /// lands in the window).
    BeforeLock,
    /// `before-write`: after the rewrite's read, before its conditional
    /// write, inside the object's locks (a write from another instance
    /// lands in the window).
    BeforeWrite,
}

/// The parsed `DGP_TEST_REWRITE_STALL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteStallSeam {
    pub point: RewriteStall,
    pub ms: u64,
    /// The file that each stall appends the object's name to.
    pub signal: Option<std::path::PathBuf>,
}

impl RewriteStallSeam {
    /// Pure: parse `<point>:<ms>[:<signal file>]`; `None` for anything
    /// else or `ms == 0`.
    pub fn parse(v: &str) -> Option<Self> {
        let mut parts = v.trim().splitn(3, ':');
        let point = match parts.next()?.trim() {
            "before-lock" => RewriteStall::BeforeLock,
            "before-write" => RewriteStall::BeforeWrite,
            _ => return None,
        };
        let ms: u64 = parts.next()?.trim().parse().ok().filter(|ms| *ms > 0)?;
        let signal = parts
            .next()
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from);
        Some(Self { point, ms, signal })
    }
}

impl TestSeams {
    /// The rewrite stall at `point`, if the seam names this point.
    pub fn rewrite_stall_at(&self, point: RewriteStall) -> Option<&RewriteStallSeam> {
        self.rewrite_stall.as_ref().filter(|s| s.point == point)
    }

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
            maintenance_object_stall_ms: lookup_parse(env, "DGP_TEST_MAINTENANCE_OBJECT_STALL_MS")
                .unwrap_or(0),
            lifecycle_object_stall_ms: lookup_parse(env, "DGP_TEST_LIFECYCLE_OBJECT_STALL_MS")
                .unwrap_or(0),
            read_other_form_stall_ms: lookup_parse(env, "DGP_TEST_READ_OTHER_FORM_STALL_MS")
                .unwrap_or(0),
            max_job_pages: lookup_parse::<u32>(env, "DGP_TEST_MAX_JOB_PAGES").filter(|n| *n > 0),
            force_noncas_backends: env("DGP_TEST_FORCE_NONCAS_BACKEND")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            force_noncas_while_file: env("DGP_TEST_FORCE_NONCAS_WHILE_FILE")
                .filter(|v| !v.is_empty())
                .map(std::path::PathBuf::from),
            conversion_grace_secs: lookup_parse::<i64>(env, "DGP_TEST_CONVERSION_GRACE_SECS")
                .filter(|n| *n >= 0),
            conversion_window_secs: lookup_parse::<i64>(env, "DGP_TEST_CONVERSION_WINDOW_SECS")
                .filter(|n| *n > 0),
            rewrite_stall: env("DGP_TEST_REWRITE_STALL").and_then(|v| RewriteStallSeam::parse(&v)),
            keep_write_gate: lookup_parse(env, "DGP_TEST_KEEP_WRITE_GATE").unwrap_or(false),
            atomicity_probe_delay_ms: lookup_parse(env, "DGP_TEST_ATOMICITY_PROBE_DELAY_MS")
                .unwrap_or(0),
            copy_object_max_bytes: copy_seam_bytes(lookup_parse(
                env,
                "DGP_TEST_COPY_OBJECT_MAX_BYTES",
            )),
            copy_part_bytes: copy_seam_bytes(lookup_parse(env, "DGP_TEST_COPY_PART_BYTES")),
            disk_free_bytes: lookup_parse(env, "DGP_TEST_DISK_FREE_BYTES"),
            disk_monitor_free_percent: lookup_parse::<u8>(
                env,
                "DGP_TEST_DISK_MONITOR_FREE_PERCENT",
            )
            .filter(|p| *p <= 100),
        }
    }
}

/// Every `DGP_TEST_*` variable that [`TestSeams::from_env`] reads. The
/// startup warning for an exported seam lists these; the unit test
/// `the_warning_list_names_every_seam_that_from_env_reads` keeps the list
/// and the parser in step.
pub const SEAM_ENV_VARS: &[&str] = &[
    "DGP_TEST_COMPLETE_STALL_MS",
    "DGP_TEST_FAIL_PART_ONCE",
    "DGP_TEST_PART_BARRIER",
    "DGP_TEST_PART_DELAY_MS",
    "DGP_TEST_OBJECT_BARRIER",
    "DGP_TEST_OBJECT_DELAY_MS",
    "DGP_TEST_COPY_STALL_MS",
    "DGP_TEST_MAINTENANCE_OBJECT_STALL_MS",
    "DGP_TEST_LIFECYCLE_OBJECT_STALL_MS",
    "DGP_TEST_READ_OTHER_FORM_STALL_MS",
    "DGP_TEST_MAX_JOB_PAGES",
    "DGP_TEST_FORCE_NONCAS_BACKEND",
    "DGP_TEST_FORCE_NONCAS_WHILE_FILE",
    "DGP_TEST_CONVERSION_GRACE_SECS",
    "DGP_TEST_CONVERSION_WINDOW_SECS",
    "DGP_TEST_REWRITE_STALL",
    "DGP_TEST_KEEP_WRITE_GATE",
    "DGP_TEST_ATOMICITY_PROBE_DELAY_MS",
    "DGP_TEST_COPY_OBJECT_MAX_BYTES",
    "DGP_TEST_COPY_PART_BYTES",
    "DGP_TEST_DISK_FREE_BYTES",
    "DGP_TEST_DISK_MONITOR_FREE_PERCENT",
];

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

    /// The class guard: a seam that `from_env` reads but the startup warning
    /// does not list would run silently in a debug build, and a name in the
    /// list that nothing reads would be stale. The lookup answers `1` to
    /// every name, so every conditional read (`DGP_TEST_PART_DELAY_MS` only
    /// after its barrier flag) happens.
    #[test]
    fn the_warning_list_names_every_seam_that_from_env_reads() {
        let asked = std::cell::RefCell::new(BTreeSet::new());
        let _ = TestSeams::from_env(&|n: &str| {
            asked.borrow_mut().insert(n.to_string());
            Some("1".to_string())
        });
        let asked: BTreeSet<String> = asked
            .into_inner()
            .into_iter()
            .filter(|n| n.starts_with("DGP_TEST_"))
            .collect();
        let listed: BTreeSet<String> = SEAM_ENV_VARS.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            asked.difference(&listed).collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "read by TestSeams::from_env, missing from SEAM_ENV_VARS"
        );
        assert_eq!(
            listed.difference(&asked).collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "in SEAM_ENV_VARS, read by nothing"
        );
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
            ("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "250"),
            ("DGP_TEST_LIFECYCLE_OBJECT_STALL_MS", "30"),
            ("DGP_TEST_READ_OTHER_FORM_STALL_MS", "400"),
            ("DGP_TEST_MAX_JOB_PAGES", "3"),
            ("DGP_TEST_FORCE_NONCAS_BACKEND", " b2sim, ,other "),
            ("DGP_TEST_CONVERSION_GRACE_SECS", "0"),
            ("DGP_TEST_CONVERSION_WINDOW_SECS", "30"),
            ("DGP_TEST_REWRITE_STALL", "before-lock:3000"),
            ("DGP_TEST_COPY_OBJECT_MAX_BYTES", "5242880"),
            ("DGP_TEST_COPY_PART_BYTES", "6291456"),
            ("DGP_TEST_DISK_MONITOR_FREE_PERCENT", "8"),
        ]);
        assert_eq!(t.disk_monitor_free_percent, Some(8));
        assert_eq!(t.copy_object_max_bytes, Some(5 * 1024 * 1024));
        assert_eq!(t.copy_part_bytes, Some(6 * 1024 * 1024));
        assert_eq!(
            t.rewrite_stall_at(RewriteStall::BeforeLock).map(|s| s.ms),
            Some(3000)
        );
        assert_eq!(t.rewrite_stall_at(RewriteStall::BeforeWrite), None);
        assert_eq!(
            RewriteStallSeam::parse("before-write:40:/tmp/sig"),
            Some(RewriteStallSeam {
                point: RewriteStall::BeforeWrite,
                ms: 40,
                signal: Some("/tmp/sig".into()),
            })
        );
        for bad in [
            "before-write",
            "after:5",
            "before-lock:x",
            "before-lock:0",
            "",
        ] {
            assert_eq!(RewriteStallSeam::parse(bad), None, "{bad}");
        }
        assert_eq!(t.conversion_grace_secs, Some(0));
        assert_eq!(t.conversion_window_secs, Some(30));
        assert_eq!(t.complete_stall_ms, 1500);
        assert_eq!(t.fail_part_once, Some(2));
        assert_eq!(t.part_delay_ms, Some(150), "the default delay");
        assert_eq!(t.object_delay_ms, Some(20));
        assert_eq!(t.copy_stall_ms, 50);
        assert_eq!(t.maintenance_object_stall_ms, 250);
        assert_eq!(t.lifecycle_object_stall_ms, 30);
        assert_eq!(t.read_other_form_stall_ms, 400);
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
            ("DGP_TEST_CONVERSION_GRACE_SECS", "-1"),
            ("DGP_TEST_CONVERSION_WINDOW_SECS", "0"),
            // Under the 5 MiB floor: S3 would refuse such a part.
            ("DGP_TEST_COPY_OBJECT_MAX_BYTES", "5242879"),
            ("DGP_TEST_COPY_PART_BYTES", "1024"),
        ]);
        assert_eq!(t, TestSeams::default());
    }

    #[test]
    fn the_copy_seams_stay_between_the_s3_floor_and_ceiling() {
        let mib5 = COPY_SEAM_FLOOR_BYTES;
        let gib5 = COPY_SEAM_CEILING_BYTES;
        assert_eq!(copy_seam_bytes(Some(mib5 - 1)), None);
        assert_eq!(copy_seam_bytes(Some(mib5)), Some(mib5));
        assert_eq!(copy_seam_bytes(Some(gib5)), Some(gib5));
        assert_eq!(copy_seam_bytes(Some(gib5 + 1)), None);
        assert_eq!(copy_seam_bytes(None), None);
        // Through the parser, for both variables.
        let over = from(&[
            ("DGP_TEST_COPY_OBJECT_MAX_BYTES", "5368709121"),
            ("DGP_TEST_COPY_PART_BYTES", "5368709121"),
        ]);
        assert_eq!(over, TestSeams::default());
        let top = from(&[
            ("DGP_TEST_COPY_OBJECT_MAX_BYTES", "5368709120"),
            ("DGP_TEST_COPY_PART_BYTES", "5368709120"),
        ]);
        assert_eq!(top.copy_object_max_bytes, Some(gib5));
        assert_eq!(top.copy_part_bytes, Some(gib5));
    }
}
