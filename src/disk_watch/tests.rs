// SPDX-License-Identifier: BUSL-1.1

use super::*;
use proptest::prelude::*;

const GIB: u64 = 1024 * 1024 * 1024;
const TOTAL: u64 = 100 * GIB;

fn tuning() -> DiskMonitorTuning {
    DiskMonitorTuning {
        interval_secs: 30,
        low_percent: 15,
        high_percent: 10,
        critical_percent: 5,
        recover_points: 2,
        low_max_bytes: 50 * GIB,
        high_max_bytes: 25 * GIB,
        critical_max_bytes: 10 * GIB,
        measure_timeout_secs: 5,
    }
}

/// 15 / 10 / 5 GiB of 100, floors below the percents (a 1 GiB reservation:
/// 2 GiB low floor, 1 GiB high floor, 1 GiB critical floor).
fn th() -> Thresholds {
    Thresholds::new(&tuning(), GIB)
}

fn facts(free_gib: u64) -> Facts {
    Facts {
        free: free_gib * GIB,
        total: TOTAL,
    }
}

#[test]
fn the_level_table() {
    use Level::*;
    // (previous level, free GiB of 100, next level). Entry: low 15, high
    // 10, critical 5; a level ends 2 GiB above its entry.
    let table = [
        (Ok, 50, Ok),
        (Ok, 15, Ok),
        (Ok, 14, Low),
        (Ok, 9, High),
        (Ok, 4, Critical),
        // A rise jumps to the level that the free space says.
        (Low, 4, Critical),
        (Low, 9, High),
        // The band above the entry keeps the level.
        (Low, 14, Low),
        (Low, 16, Low),
        (Low, 17, Ok),
        (High, 9, High),
        (High, 11, High),
        (High, 11, High),
        // A fall is one step: High ends at 12, and 12 still says Low.
        (High, 12, Low),
        (High, 80, Low),
        (Low, 80, Ok),
        (Critical, 3, Critical),
        (Critical, 6, Critical),
        (Critical, 7, High),
        (Critical, 80, High),
    ];
    for (prev, free, want) in table {
        assert_eq!(
            next_level(prev, facts(free), &th()),
            want,
            "{prev:?} with {free} GiB free"
        );
    }
    // An unmeasured disk keeps the level.
    for prev in Level::ALL {
        assert_eq!(next_level(prev, Facts { free: 0, total: 0 }, &th()), prev);
    }
}

#[test]
fn the_floors_come_from_the_spool_limits() {
    // A 20 GiB reservation: high below 20 GiB, low below 40 GiB.
    let th = Thresholds::new(&tuning(), 20 * GIB);
    assert_eq!(th.enter(Level::Critical, TOTAL), 5 * GIB);
    assert_eq!(th.enter(Level::High, TOTAL), 20 * GIB);
    assert_eq!(th.enter(Level::Low, TOTAL), 40 * GIB);
    assert_eq!(next_level(Level::Ok, facts(39), &th), Level::Low);
    assert_eq!(next_level(Level::Ok, facts(19), &th), Level::High);
    // The critical floor is 1 GiB on a small disk.
    let small = Facts {
        free: GIB / 2,
        total: 10 * GIB,
    };
    assert_eq!(
        next_level(Level::Ok, small, &Thresholds::new(&tuning(), 0)),
        Level::Critical
    );
    // A settings mistake cannot make the levels overlap: high above low
    // raises low to meet it (the ceiling of high, 25 GiB, caps both).
    let mut t = tuning();
    t.high_percent = 30;
    let th = Thresholds::new(&t, 0);
    assert_eq!(th.enter(Level::Low, TOTAL), 25 * GIB);
    assert_eq!(th.enter(Level::High, TOTAL), 25 * GIB);
}

#[test]
fn a_ceiling_in_bytes_keeps_a_huge_disk_from_refusing_with_room_to_spare() {
    use Level::*;
    const TIB: u64 = 1024 * GIB;
    let huge = 10 * TIB;
    // (free GiB of a 10 TiB disk, level): 5% of 10 TiB is 512 GiB, but the
    // ceilings are 10 / 25 / 50 GiB.
    let table = [
        (400 * 1024 / 1024, Ok),
        (51, Ok),
        (50, Ok),
        (49, Low),
        (26, Low),
        (24, High),
        (11, High),
        (9, Critical),
        (0, Critical),
    ];
    for (free_gib, want) in table {
        let f = Facts {
            free: free_gib * GIB,
            total: huge,
        };
        assert_eq!(raw_level(f, &th()), want, "{free_gib} GiB free of 10 TiB");
    }
    // A small disk keeps its percent: 5% of 100 GiB is below every ceiling.
    assert_eq!(th().enter(Level::Critical, TOTAL), 5 * GIB);
    // A floor above a ceiling loses to the ceiling (the operator's word).
    let big_upload = Thresholds::new(&tuning(), 40 * GIB);
    assert_eq!(big_upload.enter(Level::High, huge), 25 * GIB);
    assert_eq!(big_upload.enter(Level::Low, huge), 50 * GIB);
    // A huge disk recovers within a fifth of the ceiling, not 2% of 10 TiB.
    assert_eq!(th().recover_margin(Level::Critical, huge), 2 * GIB);
    assert_eq!(th().recover_margin(Level::Low, huge), 10 * GIB);
}

#[test]
fn a_ceiling_of_zero_turns_the_level_off() {
    use Level::*;
    let f = |g: u64| Facts {
        free: g * GIB,
        total: TOTAL,
    };
    // No level critical: 1 GiB free of 100 is still only high.
    let mut t = tuning();
    t.critical_max_bytes = 0;
    let th = Thresholds::new(&t, GIB);
    assert!(!th.enabled(Critical));
    assert_eq!(raw_level(f(1), &th), High);
    assert_eq!(th.enter(Critical, TOTAL), 0);
    // No level high: the spool never refuses on the free space. Critical
    // stays, and a fall from critical skips the level that is off.
    let mut t = tuning();
    t.high_max_bytes = 0;
    let th = Thresholds::new(&t, GIB);
    assert_eq!(raw_level(f(9), &th), Low);
    assert_eq!(raw_level(f(4), &th), Critical);
    assert_eq!(next_level(Critical, f(80), &th), Low, "high is skipped");
    // All off: nothing ever happens.
    let off = DiskMonitorTuning {
        low_max_bytes: 0,
        high_max_bytes: 0,
        critical_max_bytes: 0,
        ..tuning()
    };
    let th = Thresholds::new(&off, GIB);
    for g in [0, 1, 50] {
        assert_eq!(next_level(Ok, f(g), &th), Ok);
    }
    // The use of the spool keeps an off level off, and has no ceiling.
    let th = Thresholds::percent_only(&t);
    assert!(!th.enabled(High));
    assert_eq!(th.enter(Level::Low, 2 * GIB), percent_of(2 * GIB, 15));
}

#[test]
fn log_action_logs_changes_and_reminds_every_ten_minutes_at_high() {
    let t0 = Instant::now();
    let later = |s| t0 + Duration::from_secs(s);
    use Level::*;
    use LogAction::*;
    let table = [
        (Ok, Ok, None, later(0), Quiet),
        (Ok, Low, None, later(0), Changed),
        (Low, Ok, Some(t0), later(1), Changed),
        (Low, Low, Some(t0), later(100_000), Quiet),
        (High, High, Some(t0), later(599), Quiet),
        (High, High, Some(t0), later(600), Reminder),
        (Critical, Critical, Some(t0), later(600), Reminder),
        (Critical, Critical, Some(t0), later(10), Quiet),
        (High, Critical, Some(t0), later(10), Changed),
        (High, High, None, later(0), Reminder),
    ];
    for (prev, next, last, now, want) in table {
        assert_eq!(
            log_action(prev, next, last, now),
            want,
            "{prev:?} -> {next:?} last {last:?} now {now:?}"
        );
    }
}

fn any_level() -> impl Strategy<Value = Level> {
    (0u8..4).prop_map(Level::from_u8)
}

fn any_thresholds() -> impl Strategy<Value = Thresholds> {
    (
        1u8..=50,
        1u8..=50,
        1u8..=50,
        1u8..=50,
        0u64..(8 * GIB),
        0u64..(8 * GIB),
    )
        .prop_map(|(low, high, critical, recover, floor_a, floor_b)| {
            Thresholds {
                low_percent: low,
                high_percent: high,
                critical_percent: critical,
                recover_points: recover,
                low_floor: floor_a,
                high_floor: floor_b,
                critical_floor: CRITICAL_FLOOR_BYTES,
                low_max: 50 * GIB,
                high_max: 25 * GIB,
                critical_max: 10 * GIB,
            }
            .monotone()
        })
}

/// Thresholds with any ceilings, including `0` (level off).
fn any_thresholds_with_ceilings() -> impl Strategy<Value = Thresholds> {
    let ceiling = prop_oneof![Just(0u64), 1u64..(200 * GIB)];
    (any_thresholds(), ceiling.clone(), ceiling.clone(), ceiling).prop_map(
        |(th, low, high, critical)| {
            Thresholds {
                low_max: low,
                high_max: high,
                critical_max: critical,
                ..th
            }
            .monotone()
        },
    )
}

proptest! {
    /// The thresholds of the levels that are on never overlap, whatever the
    /// settings, and no level starts above its ceiling.
    #[test]
    fn the_entry_thresholds_do_not_overlap(
        th in any_thresholds_with_ceilings(),
        total in 1u64..(10_000 * GIB),
    ) {
        let on: Vec<Level> = [Level::Low, Level::High, Level::Critical]
            .into_iter()
            .filter(|l| th.enabled(*l))
            .collect();
        for pair in on.windows(2) {
            prop_assert!(th.enter(pair[0], total) >= th.enter(pair[1], total));
        }
        for l in &on {
            prop_assert!(th.enter(*l, total) <= th.ceiling(*l), "{:?}", l);
        }
        for l in [Level::Low, Level::High, Level::Critical] {
            if !th.enabled(l) {
                prop_assert_eq!(th.enter(l, total), 0);
            }
        }
    }

    /// More free space never makes a level worse, and a disk with more free
    /// space than every ceiling is never at a level: a 10 TiB volume with
    /// 400 GiB free refuses nothing.
    #[test]
    fn the_level_is_monotone_in_free_space_and_never_above_the_ceilings(
        th in any_thresholds_with_ceilings(),
        total in 1u64..(100_000 * GIB),
        a in 0u64..(100_000 * GIB),
        b in 0u64..(100_000 * GIB),
    ) {
        let (less, more) = (a.min(b).min(total), a.max(b).min(total));
        let at = |free| raw_level(Facts { free, total }, &th);
        prop_assert!(at(more) <= at(less), "{} vs {}", more, less);
        let top = th.low_max.max(th.high_max).max(th.critical_max);
        if more >= top {
            prop_assert_eq!(at(more), Level::Ok);
        }
    }

    /// A rise reaches the level that the free space says; a fall is one
    /// step, and only at the entry threshold plus the margin; else the
    /// level stays.
    #[test]
    fn a_level_moves_by_the_rule(
        th in any_thresholds_with_ceilings(),
        prev in any_level(),
        total in 1u64..(10_000 * GIB),
        free_per_mille in 0u64..=1000,
    ) {
        let free = (u128::from(total) * u128::from(free_per_mille) / 1000) as u64;
        let f = Facts { free, total };
        let next = next_level(prev, f, &th);
        let raw = raw_level(f, &th);
        if raw > prev {
            prop_assert_eq!(next, raw, "a rise jumps to the measured level");
        } else if next < prev {
            prop_assert!(next < prev, "a fall");
            prop_assert_eq!(th.lower(prev), next, "a fall is one step, over a level that is off");
            prop_assert!(free >= th.enter(prev, total) + th.recover_margin(prev, total));
            prop_assert!(next >= raw);
        } else {
            prop_assert_eq!(next, prev);
        }
    }

    /// No flapping: a value that moves inside the band between the entry
    /// threshold of the level and the entry threshold plus the margin
    /// never changes the level.
    #[test]
    fn a_value_inside_the_hysteresis_band_never_changes_the_level(
        th in any_thresholds(),
        level in 1u8..4,
        total in (100 * GIB)..(10_000 * GIB),
        walk in proptest::collection::vec(0u64..1000, 1..40),
    ) {
        let level = Level::from_u8(level);
        prop_assume!(th.enabled(level));
        let enter = th.enter(level, total);
        let margin = th.recover_margin(level, total);
        prop_assume!(margin > 0);
        let mut current = level;
        for step in walk {
            let free = enter + margin * step / 1000;
            prop_assume!(free < enter + margin);
            current = next_level(current, Facts { free, total }, &th);
            prop_assert_eq!(current, level);
        }
    }

    /// With the same measure again and again the level settles, never
    /// above the measured level plus the band, in at most three steps.
    #[test]
    fn a_constant_measure_settles(
        th in any_thresholds(),
        prev in any_level(),
        total in 1u64..(10_000 * GIB),
        free_per_mille in 0u64..=1000,
    ) {
        let free = (u128::from(total) * u128::from(free_per_mille) / 1000) as u64;
        let f = Facts { free, total };
        let mut level = prev;
        for _ in 0..3 {
            level = next_level(level, f, &th);
        }
        prop_assert_eq!(next_level(level, f, &th), level);
        let raw = raw_level(f, &th);
        prop_assert!(level >= raw);
        if level > raw {
            prop_assert!(free < th.enter(level, total) + th.recover_margin(level, total));
        }
    }
}

// ---- the monitor ----

struct FakeProbe(Mutex<HashMap<PathBuf, PathFacts>>);

impl FakeProbe {
    fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
    fn set(&self, path: &str, device: u64, free_gib: u64) {
        self.0.lock().insert(
            PathBuf::from(path),
            PathFacts {
                free: free_gib * GIB,
                total: TOTAL,
                device,
            },
        );
    }
}

impl DiskProbe for FakeProbe {
    fn measure(&self, path: &Path) -> Option<PathFacts> {
        self.0.lock().get(path).copied()
    }
}

fn targets(with_backend: bool) -> Targets {
    Targets {
        spool_by_use: false,
        spool_dir: "/spool".into(),
        backends: if with_backend {
            vec![("hetzner-fsn1".to_string(), PathBuf::from("/backend"))]
        } else {
            Vec::new()
        },
        data_dir: "/data".into(),
        default_backend: "hetzner-fsn1".into(),
    }
}

fn targets_by_use() -> Targets {
    Targets {
        spool_by_use: true,
        ..targets(false)
    }
}

const SIZING: Sizing = Sizing {
    budget: 8 * GIB,
    used: 3 * GIB,
    largest_reservation: GIB,
};

fn backend(name: &str) -> crate::storage::BucketHome {
    crate::storage::BucketHome::Backend(name.to_string())
}

fn level_gauge(m: &Metrics, role: &str) -> i64 {
    m.disk_level.with_label_values(&[role]).get()
}

#[test]
fn the_monitor_goes_low_high_critical_and_recovers_one_step_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 8 * 1024 * 1024).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    let metrics = Metrics::new();
    let probe = FakeProbe::new();
    // The spool and the data directory share device 1; the backend has 2.
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    probe.set("/backend", 2, 50);
    let t0 = Instant::now();
    let tick = |n: u64| {
        watch.tick(
            &probe,
            &targets(true),
            &tuning(),
            SIZING,
            t0 + Duration::from_secs(30 * n),
            Some(&metrics),
        )
    };
    let free_of_spool = |g: u64| {
        probe.set("/spool", 1, g);
        probe.set("/data", 1, g);
    };

    // ok: the gauges carry the numbers, every role has its series.
    let roles = tick(0);
    assert_eq!(roles.len(), 3);
    assert_eq!(metrics.spool_dir_free_bytes.get(), (50 * GIB) as i64);
    assert_eq!(metrics.spool_dir_total_bytes.get(), TOTAL as i64);
    assert_eq!(
        metrics.disk_free_bytes.with_label_values(&["data"]).get(),
        (50 * GIB) as i64
    );
    assert_eq!(
        metrics
            .disk_total_bytes
            .with_label_values(&["backend:hetzner-fsn1"])
            .get(),
        TOTAL as i64
    );
    assert_eq!(metrics.spool_budget_bytes.get(), (8 * GIB) as i64);
    assert_eq!(metrics.spool_used_bytes.get(), (3 * GIB) as i64);
    for role in ["spool", "backend:hetzner-fsn1", "data"] {
        assert_eq!(level_gauge(&metrics, role), 0, "{role}");
    }
    assert!(spool.admit_new_write().is_ok());

    // low: a warning level, no refusal anywhere.
    free_of_spool(14);
    tick(1);
    assert_eq!(level_gauge(&metrics, "spool"), 1);
    assert_eq!(level_gauge(&metrics, "data"), 1, "one level per device");
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 0);
    assert!(spool.admit_new_write().is_ok());
    assert_eq!(watch.write_refusal_for(&backend("hetzner-fsn1")), None);

    // high: the spool refuses new reservations, with the level in the
    // message, as the retryable CONTENDED kind.
    free_of_spool(9);
    tick(2);
    assert_eq!(level_gauge(&metrics, "spool"), 2);
    let e = spool.admit_new_write().unwrap_err();
    assert_eq!(e.kind(), crate::deltaglider::spool::CONTENDED);
    assert!(e.to_string().contains("level high"), "{e}");
    assert!(e.to_string().contains("/spool"), "{e}");
    assert!(e.to_string().contains("DGP_SPOOL_MAX_BYTES"), "{e}");
    // A device at high does not stop the writes of another device.
    assert_eq!(watch.write_refusal_for(&backend("hetzner-fsn1")), None);

    // critical on the spool device only: the backend is on another disk.
    free_of_spool(4);
    tick(3);
    assert_eq!(level_gauge(&metrics, "spool"), 3);
    assert_eq!(watch.write_refusal_for(&backend("hetzner-fsn1")), None);

    // critical on the backend device: writes to its buckets are refused,
    // also a bucket with no route (the default backend).
    probe.set("/backend", 2, 3);
    tick(4);
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 3);
    let m = watch.write_refusal_for(&backend("hetzner-fsn1")).unwrap();
    assert!(m.contains("hetzner-fsn1") && m.contains("critical"), "{m}");
    assert!(watch
        .write_refusal_for(&crate::storage::BucketHome::Default)
        .is_some());
    assert_eq!(
        watch.write_refusal_for(&backend("aws-dr")),
        None,
        "another backend"
    );

    // recovery: one step per tick, the refusals end at the right step.
    free_of_spool(50);
    probe.set("/backend", 2, 50);
    tick(5);
    assert_eq!(level_gauge(&metrics, "spool"), 2, "critical -> high");
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 2);
    assert_eq!(watch.write_refusal_for(&backend("hetzner-fsn1")), None);
    assert!(spool.admit_new_write().is_err());
    tick(6);
    assert_eq!(level_gauge(&metrics, "spool"), 1, "high -> low");
    assert!(spool.admit_new_write().is_ok(), "low admits again");
    tick(7);
    assert_eq!(level_gauge(&metrics, "spool"), 0, "low -> ok");

    // A role that is gone loses its series.
    watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        SIZING,
        t0,
        Some(&metrics),
    );
    let text = prometheus::TextEncoder::new()
        .encode_to_string(&metrics.registry.gather())
        .unwrap();
    assert!(!text.contains("backend:hetzner-fsn1"), "{text}");
    assert!(text.contains("deltaglider_disk_level{path_role=\"spool\"} 0"));
}

#[test]
fn a_value_that_moves_in_the_band_does_not_flap_the_monitor() {
    let dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 8 * 1024 * 1024).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    let probe = FakeProbe::new();
    probe.set("/data", 1, 50);
    probe.set("/spool", 1, 9);
    let t0 = Instant::now();
    let mut n = 0;
    let mut tick = |free: u64| {
        probe.set("/spool", 1, free);
        n += 1;
        watch.tick(
            &probe,
            &targets(false),
            &tuning(),
            SIZING,
            t0 + Duration::from_secs(30 * n),
            None,
        )[0]
        .level
    };
    assert_eq!(tick(9), Level::High);
    // 10 to 11 GiB is above the entry of high, below its end (12).
    for free in [10, 11, 10, 11, 10, 9, 11] {
        assert_eq!(tick(free), Level::High, "{free}");
        assert!(spool.admit_new_write().is_err());
    }
    assert_eq!(tick(12), Level::Low);
}

#[test]
fn a_failed_measure_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 8 * 1024 * 1024).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 9);
    probe.set("/data", 1, 9);
    watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        SIZING,
        Instant::now(),
        None,
    );
    assert!(spool.admit_new_write().is_err());
    // The probe now fails for every path.
    probe.0.lock().clear();
    let roles = watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        SIZING,
        Instant::now(),
        None,
    );
    // Only the use of the budget is measured now; the disk keeps its verdict.
    assert!(
        roles.iter().all(|r| r.label == SPOOL_BUDGET_ROLE),
        "{roles:?}"
    );
    assert_eq!(watch.spool_level(), Level::High, "the verdict stays");
}

#[test]
fn the_messages_name_the_path_the_numbers_and_the_setting() {
    let f = Facts {
        free: GIB,
        total: TOTAL,
    };
    let m = spool_refusal_message(Level::High, "spool", Path::new("/var/dgp-spool"), f);
    for needle in [
        "/var/dgp-spool",
        "level high",
        "1 GiB free of 100 GiB",
        "DGP_SPOOL_MAX_BYTES",
        "DGP_SPOOL_DIR",
    ] {
        assert!(m.contains(needle), "{needle} in {m}");
    }
    let m = spool_refusal_message(
        Level::High,
        SPOOL_BUDGET_ROLE,
        Path::new("/var/dgp-spool"),
        f,
    );
    for needle in [
        "/var/dgp-spool",
        "level high",
        "nearly all reserved",
        "DGP_SPOOL_MAX_BYTES",
    ] {
        assert!(m.contains(needle), "{needle} in {m}");
    }
    let r = RoleReport {
        label: "backend:hetzner-fsn1".into(),
        path: "/data/hetzner".into(),
        facts: f,
        level: Level::Low,
        device: 1,
    };
    let line = log_line(Level::Low, &[&r], false);
    for needle in [
        "[disk] level low",
        "backend:hetzner-fsn1",
        "/data/hetzner",
        "add disk space",
    ] {
        assert!(line.contains(needle), "{needle} in {line}");
    }
    assert!(log_line(Level::High, &[&r], true).contains("(still)"));
}

/// The gauges are documented where an operator looks them up.
#[test]
fn the_reference_page_lists_every_disk_gauge() {
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/product/reference/metrics.md"),
    )
    .unwrap();
    for name in [
        "deltaglider_spool_budget_bytes",
        "deltaglider_spool_used_bytes",
        "deltaglider_spool_dir_free_bytes",
        "deltaglider_spool_dir_total_bytes",
        "deltaglider_disk_free_bytes",
        "deltaglider_disk_total_bytes",
        "deltaglider_disk_level",
    ] {
        assert!(
            doc.contains(&format!("`{name}`")),
            "{name} missing in metrics.md"
        );
    }
}

/// `/_/ready` does not read the disk level: a full disk still serves reads.
#[test]
fn readiness_does_not_read_the_disk_level() {
    let status = crate::source_scan::read("src/api/handlers/status.rs");
    let ready = status
        .split("#[cfg(test)]")
        .next()
        .expect("production part of status.rs");
    assert!(
        !ready.contains("disk_watch"),
        "readiness must not depend on the disk level"
    );
    let health = crate::source_scan::read("src/coordination/health.rs");
    assert!(!health.contains("disk_watch"));
}

// ---- the spool judged by its use (statvfs hides the real limit) ----

/// Under Kubernetes the free space of an `emptyDir` with a `sizeLimit` is the
/// node's, so the spool is also judged by the bytes reserved against the
/// budget: percent thresholds only, no floor and no ceiling in bytes. The
/// judgement is LIVE at every new reservation (no tick between a spike and
/// its end): a spike that ends admits at once.
#[test]
fn the_spool_is_judged_live_by_its_use_where_the_free_space_hides_the_limit() {
    const MIB: u64 = 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let spool = crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 100 * MIB).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    let metrics = Metrics::new();
    let probe = FakeProbe::new();
    // The node disk is nearly empty: the free space says ok.
    probe.set("/spool", 1, 90);
    probe.set("/data", 1, 90);
    let sizing = |used| Sizing {
        budget: 100 * MIB,
        used,
        largest_reservation: 50 * MIB,
    };
    let tick = |used| {
        watch.tick(
            &probe,
            &targets_by_use(),
            &tuning(),
            sizing(used),
            Instant::now(),
            Some(&metrics),
        )
    };
    // Before the first tick nothing is judged by use; after it the gate is on.
    assert!(spool.gate().by_use().is_none());
    tick(0);
    assert!(spool.gate().by_use().is_some());
    assert_eq!(level_gauge(&metrics, SPOOL_BUDGET_ROLE), 0);

    // One upload at the per-upload limit (half the budget): no refusal,
    // whatever the byte floors say.
    let half = spool.try_acquire(50 * MIB).unwrap();
    assert!(spool.admit_new_write().is_ok());
    // 91% reserved (9% left, below the 10% of high): refused NOW, with no
    // tick in between, with the budget message.
    let more = spool.try_acquire(41 * MIB).unwrap();
    let e = spool.admit_new_write().unwrap_err();
    assert_eq!(e.kind(), crate::deltaglider::spool::CONTENDED);
    assert!(e.to_string().contains("nearly all reserved"), "{e}");
    assert!(e.to_string().contains("DGP_SPOOL_MAX_BYTES"), "{e}");
    // The gauge follows at the next tick; the refusal does not wait for it.
    tick(91 * MIB);
    assert_eq!(level_gauge(&metrics, SPOOL_BUDGET_ROLE), 2);
    // The spike ends: admitted at once, while the monitor's last verdict (a
    // tick ago) still says high.
    drop(more);
    assert!(
        spool.admit_new_write().is_ok(),
        "no tick is needed to admit"
    );
    assert_eq!(level_gauge(&metrics, SPOOL_BUDGET_ROLE), 2);
    drop(half);
    // The use of the budget has no free/total series of its own.
    let text = prometheus::TextEncoder::new()
        .encode_to_string(&metrics.registry.gather())
        .unwrap();
    assert!(!text.contains("deltaglider_disk_free_bytes{path_role=\"spool-budget\""));
    // A tick without the need drops the live judgement.
    watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        sizing(0),
        Instant::now(),
        None,
    );
    assert!(spool.gate().by_use().is_none());
}

/// Pure: the live refusal over `used / budget`, by table.
#[test]
fn the_use_refusal_table() {
    const MIB: u64 = 1024 * 1024;
    let th = Thresholds::percent_only(&tuning());
    let dir = Path::new("/spool");
    // (used MiB of 100, refused?): high starts below 10% left.
    let table = [
        (0, false),
        (50, false),
        (85, false),
        (89, false),
        (90, false),
        (91, true),
        (96, true),
        (100, true),
        (150, true),
    ];
    for (used, want) in table {
        let got = use_refusal(&th, 100 * MIB, used * MIB, dir);
        assert_eq!(got.is_some(), want, "{used} MiB of 100 reserved");
    }
    // No budget, no judgement (a spool of zero is not a verdict).
    assert!(use_refusal(&th, 0, 0, dir).is_none());
    // A level high that is off never refuses; critical alone refuses at 95%.
    let mut t = tuning();
    t.high_max_bytes = 0;
    let th = Thresholds::percent_only(&t);
    assert!(use_refusal(&th, 100 * MIB, 92 * MIB, dir).is_none());
    assert!(use_refusal(&th, 100 * MIB, 96 * MIB, dir).is_some());
    t.critical_max_bytes = 0;
    let th = Thresholds::percent_only(&t);
    assert!(use_refusal(&th, 100 * MIB, 100 * MIB, dir).is_none());
}

/// The spool level is the worse of its disk and its use; a spool whose disk
/// cannot be measured is judged by its use alone.
#[test]
fn the_spool_level_is_the_worse_of_its_disk_and_its_use() {
    let dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 8 * 1024 * 1024).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    let probe = FakeProbe::new();
    probe.set("/data", 1, 90);
    let sizing = Sizing {
        budget: 10 * GIB,
        used: 0,
        largest_reservation: GIB,
    };
    // No measure of /spool at all: the use (0) says ok and the role exists.
    let roles = watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        sizing,
        Instant::now(),
        None,
    );
    assert!(roles.iter().any(|r| r.label == SPOOL_BUDGET_ROLE));
    assert_eq!(watch.spool_level(), Level::Ok);
    assert!(spool.admit_new_write().is_ok());
    // The disk is fine, the use is high (live, from the real spool): refused.
    let held = spool.try_acquire(7 * 1024 * 1024 + 512 * 1024).unwrap();
    assert!(spool.admit_new_write().is_err(), "the use refuses");
    drop(held);
    assert!(spool.admit_new_write().is_ok());
    // The disk is critical, the use is fine: the disk refuses.
    probe.set("/spool", 2, 3);
    watch.tick(
        &probe,
        &targets_by_use(),
        &tuning(),
        sizing,
        Instant::now(),
        None,
    );
    assert_eq!(watch.spool_level(), Level::Critical);
    assert!(spool.admit_new_write().is_err(), "the disk refuses");
}

#[test]
fn the_use_thresholds_have_no_floor_in_bytes() {
    let th = Thresholds::percent_only(&tuning());
    let budget = 2 * GIB;
    assert_eq!(th.enter(Level::Critical, budget), percent_of(budget, 5));
    assert_eq!(th.enter(Level::High, budget), percent_of(budget, 10));
    assert_eq!(th.enter(Level::Low, budget), percent_of(budget, 15));
}

#[test]
fn the_view_of_the_spool_by_what_the_boot_found() {
    use SpoolView::*;
    use UseReason::*;
    // (tmpfs, kubernetes, measured) -> view
    let table = [
        (false, false, true, Disk),
        (true, false, true, DiskAndUse(Tmpfs)),
        (false, true, true, DiskAndUse(Kubernetes)),
        (true, true, true, DiskAndUse(Tmpfs)),
        (false, false, false, DiskAndUse(Unmeasured)),
        (true, true, false, DiskAndUse(Unmeasured)),
    ];
    for (tmpfs, k8s, measured, want) in table {
        assert_eq!(
            spool_view(tmpfs, k8s, measured),
            want,
            "{tmpfs} {k8s} {measured}"
        );
    }
}

#[test]
fn the_boot_check_table() {
    use crate::config::limits::{FloorReason, LimitWarning as W};
    use BootSpaceVerdict::*;
    let dir = Path::new("/var/dgp-spool");
    let budget = 20 * GIB;
    // (view, free, verdict)
    let table = [
        (SpoolView::Disk, Some(20 * GIB), Fits),
        (
            SpoolView::Disk,
            Some(20 * GIB - 1),
            BudgetAboveFree {
                budget,
                free: 20 * GIB - 1,
            },
        ),
        (
            SpoolView::Disk,
            Some(0),
            BudgetAboveFree { budget, free: 0 },
        ),
        (SpoolView::Disk, Some(500 * GIB), Fits),
        (SpoolView::Disk, None, NotVisible(UseReason::Unmeasured)),
        (
            SpoolView::DiskAndUse(UseReason::Tmpfs),
            Some(500 * GIB),
            NotVisible(UseReason::Tmpfs),
        ),
        (
            SpoolView::DiskAndUse(UseReason::Kubernetes),
            Some(0),
            NotVisible(UseReason::Kubernetes),
        ),
    ];
    for (view, free, want) in table {
        assert_eq!(
            boot_space_verdict(view, free, budget),
            want,
            "{view:?} {free:?}"
        );
    }
    // The warning names the directory, both sizes and the setting.
    let m = boot_check(SpoolView::Disk, Some(5 * GIB), budget, false, dir, &[]).unwrap();
    for needle in [
        "/var/dgp-spool",
        "5 GiB free",
        "20 GiB",
        "DGP_SPOOL_MAX_BYTES",
        "ENOSPC",
    ] {
        assert!(m.contains(needle), "{needle} in {m}");
    }
    assert!(boot_check(SpoolView::Disk, Some(budget), budget, false, dir, &[]).is_none());
    let m = boot_check(
        SpoolView::DiskAndUse(UseReason::Kubernetes),
        None,
        budget,
        false,
        dir,
        &[],
    )
    .unwrap();
    assert!(
        m.contains("Kubernetes") && m.contains("DGP_SPOOL_MAX_BYTES"),
        "{m}"
    );
    // The derivation of the limits warned already: no second line.
    let above = W::SpoolAboveFreeSpace {
        budget,
        available: 5 * GIB,
        shared_device: false,
    };
    let floor = W::FloorAboveAvailable { available: 5 * GIB };
    for w in [above, floor] {
        assert!(boot_check(SpoolView::Disk, Some(5 * GIB), budget, false, dir, &[w]).is_none());
    }
    let unreliable = W::FreeSpaceUnreliable {
        reason: FloorReason::Kubernetes,
    };
    assert!(boot_check(
        SpoolView::DiskAndUse(UseReason::Kubernetes),
        None,
        budget,
        false,
        dir,
        &[unreliable]
    )
    .is_none());
    // A warning of another kind does not hide it.
    assert!(boot_check(
        SpoolView::Disk,
        Some(5 * GIB),
        budget,
        false,
        dir,
        &[unreliable]
    )
    .is_some());
}

/// An EXPLICIT budget on a directory whose free space is no proof (a
/// Kubernetes `emptyDir`, a tmpfs, a failed measure) is the right setup
/// there: no warning about a budget that nobody set. An explicit budget
/// above the free space of a visible disk still warns.
#[test]
fn an_explicit_budget_on_a_hidden_limit_does_not_warn() {
    let dir = Path::new("/var/dgp-spool");
    let budget = 20 * GIB;
    for reason in [
        UseReason::Kubernetes,
        UseReason::Tmpfs,
        UseReason::Unmeasured,
    ] {
        let view = SpoolView::DiskAndUse(reason);
        assert!(
            boot_check(view, None, budget, true, dir, &[]).is_none(),
            "{reason:?} with an explicit budget"
        );
        assert!(
            boot_check(view, Some(500 * GIB), budget, true, dir, &[]).is_none(),
            "{reason:?} with an explicit budget and a measured disk"
        );
        // The same view with an auto budget (and no earlier warning) speaks.
        assert!(boot_check(view, None, budget, false, dir, &[]).is_some());
    }
    assert!(boot_check(SpoolView::Disk, Some(5 * GIB), budget, true, dir, &[]).is_some());
}

proptest! {
    /// The boot verdict is Fits exactly when a measured disk holds the budget.
    #[test]
    fn the_boot_verdict_is_fits_exactly_when_the_disk_holds_the_budget(
        free in 0u64..(1000 * GIB), budget in 0u64..(1000 * GIB)
    ) {
        let v = boot_space_verdict(SpoolView::Disk, Some(free), budget);
        prop_assert_eq!(v == BootSpaceVerdict::Fits, free >= budget);
    }
}

// ---- review round 1 ----

fn watch_and_spool() -> (
    tempfile::TempDir,
    crate::deltaglider::spool::SpoolDir,
    DiskWatch,
) {
    let dir = tempfile::tempdir().unwrap();
    let spool =
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("s"), 8 * 1024 * 1024).unwrap();
    let watch = DiskWatch::new(spool.gate().clone());
    (dir, spool, watch)
}

/// A failed measure changes nothing for ITS role: not the level, not the
/// refusal of a backend at level critical, not the series. A role that the
/// config no longer names loses its series.
#[test]
fn a_failed_measure_keeps_the_backend_refusal_the_level_and_the_series() {
    let (_dir, _spool, watch) = watch_and_spool();
    let metrics = Metrics::new();
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    probe.set("/backend", 2, 3);
    let tick = |n: u64| {
        watch.tick(
            &probe,
            &targets(true),
            &tuning(),
            SIZING,
            Instant::now() + Duration::from_secs(30 * n),
            Some(&metrics),
        )
    };
    tick(0);
    assert!(watch.write_refusal_for(&backend("hetzner-fsn1")).is_some());
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 3);

    // The backend's measure fails (a hung or unplugged mount).
    probe.0.lock().remove(Path::new("/backend"));
    let roles = tick(1);
    assert!(roles.iter().all(|r| r.label != "backend:hetzner-fsn1"));
    assert!(
        watch.write_refusal_for(&backend("hetzner-fsn1")).is_some(),
        "the refusal stays"
    );
    assert_eq!(
        level_gauge(&metrics, "backend:hetzner-fsn1"),
        3,
        "the series stays"
    );
    let text = prometheus::TextEncoder::new()
        .encode_to_string(&metrics.registry.gather())
        .unwrap();
    assert!(text.contains("deltaglider_disk_free_bytes{path_role=\"backend:hetzner-fsn1\"}"));

    // The measure answers again with room: the refusal ends one step at a time.
    probe.set("/backend", 2, 50);
    tick(2);
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 2);
    assert!(watch.write_refusal_for(&backend("hetzner-fsn1")).is_none());

    // The backend leaves the config: its series go with it.
    watch.tick(
        &probe,
        &targets(false),
        &tuning(),
        SIZING,
        Instant::now(),
        Some(&metrics),
    );
    let text = prometheus::TextEncoder::new()
        .encode_to_string(&metrics.registry.gather())
        .unwrap();
    assert!(!text.contains("backend:hetzner-fsn1"), "{text}");
}

/// The monitor logs on a level change, and again only as a reminder while
/// the level is high or critical: not at every tick, not at level ok.
#[test]
fn the_monitor_logs_on_a_level_change_only() {
    let (_dir, _spool, watch) = watch_and_spool();
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    let t0 = Instant::now();
    let mut n = 0u64;
    let mut tick = |free_gib: u64, secs: u64| {
        probe.set("/spool", 1, free_gib);
        probe.set("/data", 1, free_gib);
        n += 1;
        let _ = n;
        watch.tick(
            &probe,
            &targets(false),
            &tuning(),
            SIZING,
            t0 + Duration::from_secs(secs),
            None,
        );
        watch.emitted.lock().len()
    };
    assert_eq!(tick(50, 0), 0, "ok at the start: no line");
    assert_eq!(tick(50, 30), 0, "ok again: no line");
    assert_eq!(tick(14, 60), 1, "ok -> low: one line");
    assert_eq!(tick(14, 90), 1, "low again: quiet");
    assert_eq!(tick(9, 120), 2, "low -> high: one line");
    assert_eq!(tick(9, 150), 2, "high again, soon: quiet");
    assert_eq!(tick(9, 120 + 600), 3, "high for ten minutes: a reminder");
    assert_eq!(tick(9, 120 + 630), 3, "then quiet again");
    assert_eq!(tick(4, 120 + 660), 4, "high -> critical: one line");
    let lines = watch.emitted.lock().clone();
    assert!(lines[2].1.contains("(still)"), "{}", lines[2].1);
    assert_eq!(
        lines.iter().map(|(l, _)| *l).collect::<Vec<_>>(),
        [Level::Low, Level::High, Level::High, Level::Critical]
    );
    assert!(lines[0].1.contains("level low") && lines[0].1.contains("/spool"));
}

/// THE running tick (`tick_app`) reads the config: a filesystem backend that
/// is the default one is refused by name for a bucket with no route.
#[tokio::test]
async fn the_running_tick_names_the_default_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let fs_backend: Box<crate::storage::DynStorageBackend<'static>> =
        crate::storage::DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(data.clone())
                .await
                .unwrap(),
        );
    // A spool of its own: the process-wide gate must not feel this test.
    let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
        Arc::new(fs_backend),
        &crate::config::Config::default(),
        None,
    )
    .with_spool_dir(
        crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 8 * 1024 * 1024)
            .unwrap(),
    );
    let state = crate::api::handlers::AppState::for_tests(engine);
    let mut config = crate::config::Config::default();
    config.backend = crate::config::BackendConfig::Filesystem { path: data.clone() };
    let watch = DiskWatch::new(state.engine.load().spool_dir().gate().clone());
    let probe = FakeProbe::new();
    probe.set(data.to_str().unwrap(), 7, 3); // the backend disk: 3% free
    tick_app(&watch, &probe, &state, &config);
    let name = config.default_backend_name();
    assert_eq!(name, "default");
    let m = watch
        .write_refusal_for(&crate::storage::BucketHome::Default)
        .expect("a bucket with no route lives on the default backend");
    assert!(m.contains("backend 'default'"), "{m}");
    assert!(watch.write_refusal_for(&backend("other")).is_none());
}

#[test]
fn the_refusal_of_a_home_by_table() {
    use crate::storage::BucketHome;
    let critical: BTreeMap<String, String> =
        [("second".to_string(), "disk full".to_string())].into();
    let table = [
        (BucketHome::Backend("second".into()), Some("disk full")),
        (BucketHome::Backend("first".into()), None),
        // No router: the one backend, the default.
        (BucketHome::Default, None),
        // The router cannot tell: never a guess.
        (BucketHome::Undetermined, None),
    ];
    for (home, want) in table {
        assert_eq!(
            refusal_for_home(&critical, "first", &home).as_deref(),
            want,
            "{home:?}"
        );
    }
    // The default backend itself critical: `Default` and an Undetermined
    // bucket differ, the first is refused and the second is not.
    let critical: BTreeMap<String, String> = [("first".to_string(), "full".to_string())].into();
    assert!(refusal_for_home(&critical, "first", &BucketHome::Default).is_some());
    assert!(refusal_for_home(&critical, "first", &BucketHome::Undetermined).is_none());
}

/// A bucket that lives on a NON-default critical backend, with a cold
/// resolver cache: the async gate asks the router (the HeadBucket round that
/// the write itself sends) and refuses by the real backend, not the default
/// one. The memory-only gate does not guess on a cold cache and knows after
/// the router resolved.
#[tokio::test]
async fn a_bucket_on_a_non_default_critical_backend_is_refused_with_a_cold_cache() {
    use crate::storage::StorageBackend as _;
    let tmp = tempfile::tempdir().unwrap();
    let mut backends: HashMap<String, Arc<Box<crate::storage::DynStorageBackend<'static>>>> =
        HashMap::new();
    let mut paths = Vec::new();
    for name in ["first", "second"] {
        let dir = tmp.path().join(name);
        let fs = crate::storage::FilesystemBackend::new(dir.clone())
            .await
            .unwrap();
        if name == "second" {
            // The bucket exists on `second` only; no route names it.
            fs.create_bucket("b2").await.unwrap();
        } else {
            fs.create_bucket("b1").await.unwrap();
        }
        paths.push(dir);
        backends.insert(
            name.to_string(),
            Arc::new(crate::storage::DynStorageBackend::new_box(fs)),
        );
    }
    let routing =
        crate::storage::RoutingBackend::new(backends, HashMap::new(), "first".to_string()).unwrap();
    let boxed: Box<crate::storage::DynStorageBackend<'static>> =
        crate::storage::DynStorageBackend::new_box(routing);
    let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
        Arc::new(boxed),
        &crate::config::Config::default(),
        None,
    );
    // `second` is at level critical, `first` is fine.
    let (_d, _spool, watch) = watch_and_spool();
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    probe.set("/first", 2, 50);
    probe.set("/second", 3, 3);
    let mut targets = targets(false);
    targets.backends = vec![
        ("first".into(), PathBuf::from("/first")),
        ("second".into(), PathBuf::from("/second")),
    ];
    targets.default_backend = "first".into();
    watch.tick(&probe, &targets, &tuning(), SIZING, Instant::now(), None);
    assert!(watch.any_backend_critical());

    // Cold: the memory-only gate has no answer and does not guess the default.
    assert_eq!(bucket_write_refusal_now(&watch, &engine, "b2"), None);
    // The request path resolves the home and refuses by `second`.
    let m = bucket_write_refusal_with(&watch, &engine, "b2")
        .await
        .expect("b2 lives on the critical backend `second`");
    assert!(m.contains("backend 'second'"), "{m}");
    // The router remembers: the memory-only gate knows now.
    assert!(bucket_write_refusal_now(&watch, &engine, "b2").is_some());
    // A bucket on the healthy default backend is not refused.
    assert_eq!(bucket_write_refusal_with(&watch, &engine, "b1").await, None);
    // A bucket that exists nowhere is created on the default: not refused.
    assert_eq!(
        bucket_write_refusal_with(&watch, &engine, "new").await,
        None
    );
}

#[test]
fn the_advice_follows_the_role() {
    use Level::*;
    // (role, level) -> words that must be in the advice, words that must not.
    let table: &[(&str, Level, &[&str], &[&str])] = &[
        ("spool", Low, &["DGP_SPOOL_MAX_BYTES", "DGP_SPOOL_DIR"], &[]),
        (
            "spool",
            High,
            &["refuses new uploads", "DGP_SPOOL_MAX_BYTES"],
            &["backend"],
        ),
        (
            "spool-budget",
            Critical,
            &["refuses new uploads"],
            &["backend"],
        ),
        (
            "backend:hetzner-fsn1",
            High,
            &["no write is refused yet", "'hetzner-fsn1'", "critical"],
            &["DGP_SPOOL_MAX_BYTES", "spool"],
        ),
        (
            "backend:hetzner-fsn1",
            Critical,
            &[
                "writes to the buckets of backend 'hetzner-fsn1'",
                "deletes still work",
            ],
            &["DGP_SPOOL_MAX_BYTES", "spool"],
        ),
        (
            "data",
            High,
            &["no write is refused", "config database"],
            &["DGP_SPOOL_MAX_BYTES", "backend"],
        ),
        (
            "data",
            Critical,
            &["config database", "job state"],
            &["DGP_SPOOL_MAX_BYTES"],
        ),
        ("spool", Ok, &["recovered"], &[]),
    ];
    for (role, level, has, hasnt) in table {
        let a = advice(*level, role);
        for w in *has {
            assert!(a.contains(w), "{role} {level:?}: `{w}` in `{a}`");
        }
        for w in *hasnt {
            assert!(!a.contains(w), "{role} {level:?}: `{w}` not in `{a}`");
        }
    }
    // A device that holds a backend and the data directory says both, once.
    let facts = Facts {
        free: GIB,
        total: TOTAL,
    };
    let report = |label: &str| RoleReport {
        label: label.into(),
        path: format!("/{label}").into(),
        facts,
        level: High,
        device: 1,
    };
    let (b, d) = (report("backend:a"), report("data"));
    let line = log_line(High, &[&b, &d], false);
    assert!(
        line.contains("backend:a:") && line.contains("data:"),
        "{line}"
    );
    assert!(!line.contains("DGP_SPOOL_MAX_BYTES"), "{line}");
}

// ---- the deadline of a measure ----

/// A probe that answers when its gate opens (a hung mount until then).
struct StuckProbe {
    open: Arc<(Mutex<bool>, parking_lot::Condvar)>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl DiskProbe for StuckProbe {
    fn measure(&self, _path: &Path) -> Option<PathFacts> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (lock, cv) = &*self.open;
        let mut open = lock.lock();
        while !*open {
            cv.wait(&mut open);
        }
        Some(PathFacts {
            free: GIB,
            total: TOTAL,
            device: 9,
        })
    }
}

#[test]
fn a_hung_measure_times_out_keeps_one_thread_and_recovers() {
    let open = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe = TimedProbe::new(
        Arc::new(StuckProbe {
            open: open.clone(),
            calls: calls.clone(),
        }),
        Duration::from_millis(50),
    );
    let path = Path::new("/hung-nfs");
    let t = Instant::now();
    assert_eq!(probe.measure(path), None, "a hung mount: no answer");
    assert!(
        t.elapsed() < Duration::from_secs(5),
        "the monitor is not frozen"
    );
    assert_eq!(probe.hung_paths(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The next measure of the same path starts no second thread.
    assert_eq!(probe.measure(path), None);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one thread per hung path");
    // Another path is measured (and hangs on its own thread).
    assert_eq!(probe.measure(Path::new("/other")), None);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // The mount answers: the next measure of the path works again.
    *open.0.lock() = true;
    open.1.notify_all();
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.hung_paths() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(probe.hung_paths(), 0);
    assert_eq!(probe.measure(path).map(|f| f.free), Some(GIB));
}

/// A timed-out role keeps its last verdict through the monitor.
#[test]
fn a_timed_out_role_keeps_its_verdict_in_the_monitor() {
    let (_dir, _spool, watch) = watch_and_spool();
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    probe.set("/backend", 2, 3);
    let t = targets(true);
    watch.tick(&probe, &t, &tuning(), SIZING, Instant::now(), None);
    assert!(watch.write_refusal_for(&backend("hetzner-fsn1")).is_some());
    // Now the backend measure hangs behind a timed probe.
    let open = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
    let stuck = TimedProbe::new(
        Arc::new(StuckProbe {
            open: open.clone(),
            calls: Arc::default(),
        }),
        Duration::from_millis(30),
    );
    watch.tick(&stuck, &t, &tuning(), SIZING, Instant::now(), None);
    assert!(
        watch.write_refusal_for(&backend("hetzner-fsn1")).is_some(),
        "a hung mount keeps the verdict"
    );
    *open.0.lock() = true;
    open.1.notify_all();
}

/// The timeout is a setting, and the source says what the comment says.
#[test]
fn the_monitor_uses_the_timed_probe() {
    let startup = crate::source_scan::read("src/startup.rs");
    let spawn = startup
        .split("pub async fn spawn_disk_monitor")
        .nth(1)
        .expect("spawn_disk_monitor");
    let spawn = spawn.split("\n}\n").next().unwrap();
    assert!(spawn.contains("TimedProbe::new("), "{spawn}");
    assert!(
        !spawn.contains("tick_app(&watch, &SystemProbe"),
        "the monitor measures through the timed probe"
    );
}

// ---- review round 2 ----

/// A probe that panics on its first call and answers after.
struct PanicOnce(std::sync::atomic::AtomicBool);

impl DiskProbe for PanicOnce {
    fn measure(&self, _path: &Path) -> Option<PathFacts> {
        if !self.0.swap(true, Ordering::SeqCst) {
            panic!("a probe that panics");
        }
        Some(PathFacts {
            free: GIB,
            total: TOTAL,
            device: 4,
        })
    }
}

/// A measure that panics must not leave its path hung for good: the guard
/// of the measure thread frees it on every exit. The next measure works.
#[test]
fn a_panicking_measure_frees_its_path() {
    let probe = TimedProbe::new(
        Arc::new(PanicOnce(std::sync::atomic::AtomicBool::new(false))),
        Duration::from_secs(5),
    );
    let path = Path::new("/panics");
    let t = Instant::now();
    assert_eq!(probe.measure(path), None, "a panic is no answer");
    assert!(
        t.elapsed() < Duration::from_secs(4),
        "no wait for the timeout"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.hung_paths() > 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(probe.hung_paths(), 0, "the path stays hung after a panic");
    assert_eq!(probe.measure(path).map(|f| f.free), Some(GIB));
}

/// Two roles can share one path (a filesystem backend at the data
/// directory). The second measure of the path follows the first at once, so
/// the first thread must be free of the path BEFORE it answers: else the
/// second measure meets the hang flag and answers nothing, and its role has
/// no series in that tick.
#[test]
fn a_path_is_free_again_when_its_measure_answers() {
    struct Instant_;
    impl DiskProbe for Instant_ {
        fn measure(&self, _: &Path) -> Option<PathFacts> {
            Some(PathFacts {
                free: GIB,
                total: 2 * GIB,
                device: 1,
            })
        }
    }
    let probe = TimedProbe::new(Arc::new(Instant_), Duration::from_secs(5));
    let path = Path::new("/shared");
    for round in 0..2000 {
        assert!(
            probe.measure(path).is_some(),
            "first measure, round {round}"
        );
        assert!(
            probe.measure(path).is_some(),
            "the second measure of the same path met the hang flag, round {round}"
        );
    }
}

/// A spool measure that fails or times out changes nothing, like the other
/// roles: it must not turn the judgement by use on for that tick. Only a
/// spool that was NEVER measured is judged by its use.
#[test]
fn a_failed_spool_measure_does_not_turn_the_use_judgement_on() {
    let (_dir, spool, watch) = watch_and_spool();
    let probe = FakeProbe::new();
    probe.set("/spool", 1, 50);
    probe.set("/data", 1, 50);
    let tick = |t: &Targets| watch.tick(&probe, t, &tuning(), SIZING, Instant::now(), None);
    // Measured, a plain disk: no judgement by use.
    tick(&targets(false));
    assert!(spool.gate().by_use().is_none());
    // The measure now fails (a hung mount): still none, and no such role.
    probe.0.lock().remove(Path::new("/spool"));
    let roles = tick(&targets(false));
    assert!(
        spool.gate().by_use().is_none(),
        "a failed measure changed it"
    );
    assert!(roles.iter().all(|r| r.label != SPOOL_BUDGET_ROLE));
    // It answers again, then fails again with the judgement ON (a hidden
    // limit): the judgement stays on.
    probe.set("/spool", 1, 50);
    tick(&targets_by_use());
    assert!(spool.gate().by_use().is_some());
    probe.0.lock().remove(Path::new("/spool"));
    tick(&targets(false));
    assert!(spool.gate().by_use().is_some(), "the last decision stands");

    // A spool that was never measured has only its use to go by.
    let (_d2, spool2, watch2) = watch_and_spool();
    let probe2 = FakeProbe::new();
    probe2.set("/data", 1, 50);
    watch2.tick(
        &probe2,
        &targets(false),
        &tuning(),
        SIZING,
        Instant::now(),
        None,
    );
    assert!(spool2.gate().by_use().is_some());
}
