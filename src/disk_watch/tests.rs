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
    // raises low to meet it.
    let mut t = tuning();
    t.high_percent = 30;
    let th = Thresholds::new(&t, 0);
    assert_eq!(th.enter(Level::Low, TOTAL), 30 * GIB);
    assert_eq!(th.enter(Level::High, TOTAL), 30 * GIB);
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
        .prop_map(
            |(low, high, critical, recover, floor_a, floor_b)| Thresholds {
                low_percent: low,
                high_percent: high,
                critical_percent: critical,
                recover_points: recover,
                low_floor: floor_a,
                high_floor: floor_b,
                critical_floor: CRITICAL_FLOOR_BYTES,
            },
        )
}

proptest! {
    /// The thresholds never overlap, whatever the settings.
    #[test]
    fn the_entry_thresholds_do_not_overlap(th in any_thresholds(), total in 1u64..(10_000 * GIB)) {
        prop_assert!(th.enter(Level::Low, total) >= th.enter(Level::High, total));
        prop_assert!(th.enter(Level::High, total) >= th.enter(Level::Critical, total));
        prop_assert!(th.enter(Level::Critical, total) >= CRITICAL_FLOOR_BYTES);
    }

    /// A rise reaches the level that the free space says; a fall is one
    /// step, and only at the entry threshold plus the margin; else the
    /// level stays.
    #[test]
    fn a_level_moves_by_the_rule(
        th in any_thresholds(),
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
            prop_assert_eq!(next.as_u8() + 1, prev.as_u8(), "a fall is one step");
            prop_assert!(free >= th.enter(prev, total) + th.recover_margin(total));
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
        let enter = th.enter(level, total);
        let margin = th.recover_margin(total);
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
            prop_assert!(free < th.enter(level, total) + th.recover_margin(total));
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

const SIZING: Sizing = Sizing {
    budget: 8 * GIB,
    used: 3 * GIB,
    largest_reservation: GIB,
};

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
    let mut tick = |n: u64| {
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
    assert_eq!(
        metrics.disk_free_bytes.with_label_values(&["spool"]).get(),
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
    assert_eq!(watch.write_refusal(Some("hetzner-fsn1")), None);

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
    assert_eq!(watch.write_refusal(Some("hetzner-fsn1")), None);

    // critical on the spool device only: the backend is on another disk.
    free_of_spool(4);
    tick(3);
    assert_eq!(level_gauge(&metrics, "spool"), 3);
    assert_eq!(watch.write_refusal(Some("hetzner-fsn1")), None);

    // critical on the backend device: writes to its buckets are refused,
    // also a bucket with no route (the default backend).
    probe.set("/backend", 2, 3);
    tick(4);
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 3);
    let m = watch.write_refusal(Some("hetzner-fsn1")).unwrap();
    assert!(m.contains("hetzner-fsn1") && m.contains("critical"), "{m}");
    assert!(watch.write_refusal(None).is_some());
    assert_eq!(watch.write_refusal(Some("aws-dr")), None, "another backend");

    // recovery: one step per tick, the refusals end at the right step.
    free_of_spool(50);
    probe.set("/backend", 2, 50);
    tick(5);
    assert_eq!(level_gauge(&metrics, "spool"), 2, "critical -> high");
    assert_eq!(level_gauge(&metrics, "backend:hetzner-fsn1"), 2);
    assert_eq!(watch.write_refusal(Some("hetzner-fsn1")), None);
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
    assert!(roles.is_empty());
    assert_eq!(watch.spool_level(), Level::High, "the verdict stays");
}

#[test]
fn the_messages_name_the_path_the_numbers_and_the_setting() {
    let f = Facts {
        free: GIB,
        total: TOTAL,
    };
    let m = spool_refusal_message(Level::High, Path::new("/var/dgp-spool"), f);
    for needle in [
        "/var/dgp-spool",
        "level high",
        "1.0 GiB free of 100.0 GiB",
        "DGP_SPOOL_MAX_BYTES",
        "DGP_SPOOL_DIR",
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
        "free disk space",
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
