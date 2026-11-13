// SPDX-License-Identifier: BUSL-1.1

//! Runtime disk watermarks.
//!
//! A background monitor measures the free space of the spool directory,
//! of every filesystem backend root and of the data directory. Each device
//! has one [`Level`] (ok, low, high, critical). ONE pure function,
//! [`next_level`], moves a level with hysteresis; everything else here is
//! plumbing around it.
//!
//! Effects:
//! * high or critical on the spool device: the spool refuses NEW
//!   reservations (a write that holds no spool yet) with the retryable
//!   `503 SlowDown`. A running reservation, a read and a delete are never
//!   affected ([`SpoolGate`]).
//! * critical on the device of a filesystem backend: client writes to the
//!   buckets of that backend get `503 SlowDown`
//!   ([`DiskWatch::write_refusal`], asked by the client-write gate).
//! * `/_/ready` does not change: a full disk still serves reads.
//!
//! `statvfs` does not show every real limit: a Kubernetes `emptyDir` with a
//! `sizeLimit` and a container quota both hide it, and a tmpfs is memory.
//! Where that can be so ([`SpoolView`]), the monitor also judges the spool
//! by its own use: the bytes reserved against the budget
//! (`DGP_SPOOL_MAX_BYTES`), as one more role named `spool-budget`. The spool
//! level is the worse of the two.
//!
//! The monitor sets the gauges `deltaglider_spool_budget_bytes`,
//! `deltaglider_spool_used_bytes`, `deltaglider_spool_dir_free_bytes` and
//! `deltaglider_spool_dir_total_bytes`; per other role (`backend:<name>`,
//! `data`) `deltaglider_disk_free_bytes` and `deltaglider_disk_total_bytes`;
//! and for every role `deltaglider_disk_level`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::config::tuning::DiskMonitorTuning;
use crate::metrics::Metrics;

/// The critical floor: below this many bytes the level is critical, on a
/// disk where 5% is smaller.
pub const CRITICAL_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

/// While the level is high or critical, a log line repeats at most this
/// often.
pub const REMINDER_EVERY: Duration = Duration::from_secs(10 * 60);

/// How full a device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Level {
    Ok = 0,
    Low = 1,
    High = 2,
    Critical = 3,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Ok, Level::Low, Level::High, Level::Critical];

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(n: u8) -> Level {
        match n {
            0 => Level::Ok,
            1 => Level::Low,
            2 => Level::High,
            _ => Level::Critical,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Low => "low",
            Level::High => "high",
            Level::Critical => "critical",
        }
    }

    fn one_lower(self) -> Level {
        Level::from_u8(self.as_u8().saturating_sub(1))
    }
}

/// The free and the total bytes of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub free: u64,
    pub total: u64,
}

/// The thresholds of the levels: a percent of the disk, and a floor in
/// bytes taken from the spool limits. A level starts when the free space is
/// below the larger of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub low_percent: u8,
    pub high_percent: u8,
    pub critical_percent: u8,
    /// A level ends when the free space is this many percentage points of
    /// the disk above its entry threshold.
    pub recover_points: u8,
    /// Twice the largest reservation that the spool allows for one upload.
    pub low_floor: u64,
    /// The largest reservation that the spool allows for one upload.
    pub high_floor: u64,
    pub critical_floor: u64,
}

fn percent_of(total: u64, percent: u8) -> u64 {
    (u128::from(total) * u128::from(percent) / 100) as u64
}

impl Thresholds {
    /// The thresholds of the tuning, with `largest_reservation` the most
    /// spool that one upload can reserve now.
    pub fn new(tuning: &DiskMonitorTuning, largest_reservation: u64) -> Self {
        Self {
            low_percent: tuning.low_percent,
            high_percent: tuning.high_percent,
            critical_percent: tuning.critical_percent,
            recover_points: tuning.recover_points,
            low_floor: largest_reservation.saturating_mul(2),
            high_floor: largest_reservation,
            critical_floor: CRITICAL_FLOOR_BYTES,
        }
    }

    /// The thresholds for a measure that is no disk: the use of the spool
    /// against its budget. The budget already bounds every reservation, so
    /// no floor in bytes applies, only the percents.
    pub fn percent_only(tuning: &DiskMonitorTuning) -> Self {
        Self {
            low_floor: 0,
            high_floor: 0,
            critical_floor: 0,
            ..Self::new(tuning, 0)
        }
    }

    /// The free space below which `level` starts. Never smaller for a lower
    /// level, whatever the settings: a settings mistake (high above low)
    /// cannot make the levels overlap. `Ok` has none (0).
    pub fn enter(&self, level: Level, total: u64) -> u64 {
        let critical = percent_of(total, self.critical_percent).max(self.critical_floor);
        let high = percent_of(total, self.high_percent)
            .max(self.high_floor)
            .max(critical);
        let low = percent_of(total, self.low_percent)
            .max(self.low_floor)
            .max(high);
        match level {
            Level::Ok => 0,
            Level::Low => low,
            Level::High => high,
            Level::Critical => critical,
        }
    }

    /// The bytes above the entry threshold at which a level ends.
    pub fn recover_margin(&self, total: u64) -> u64 {
        percent_of(total, self.recover_points)
    }
}

/// The level that the free space says, with no memory of the past.
pub fn raw_level(facts: Facts, th: &Thresholds) -> Level {
    Level::ALL
        .into_iter()
        .rev()
        .find(|l| *l != Level::Ok && facts.free < th.enter(*l, facts.total))
        .unwrap_or(Level::Ok)
}

/// THE level rule. From the level `prev` and a new measure:
/// * the free space is below the entry threshold of a worse level: the
///   level jumps to the level that the free space says (a disk that fills
///   fast must not wait three ticks);
/// * else the level falls ONE step, and only when the free space is at
///   least `recover_points` of the disk above the entry threshold of the
///   current level (one step per tick, so a recovery shows in the gauge
///   and the log at every step);
/// * else it stays. A value inside the band between the entry threshold
///   and the entry threshold plus the margin never changes the level, so a
///   value that moves in the band does not flap.
///
/// A disk of unknown size (`total` = 0) keeps the level.
pub fn next_level(prev: Level, facts: Facts, th: &Thresholds) -> Level {
    if facts.total == 0 {
        return prev;
    }
    let raw = raw_level(facts, th);
    if raw > prev {
        return raw;
    }
    if prev == Level::Ok {
        return Level::Ok;
    }
    let leave_at = th
        .enter(prev, facts.total)
        .saturating_add(th.recover_margin(facts.total));
    if facts.free >= leave_at {
        prev.one_lower()
    } else {
        prev
    }
}

/// What the monitor does about a tick for one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogAction {
    Quiet,
    /// The level changed: one line.
    Changed,
    /// The level is high or critical and the last line is old.
    Reminder,
}

/// Pure: log on a level change, and remind every [`REMINDER_EVERY`] while
/// the level is high or critical.
pub fn log_action(
    prev: Level,
    next: Level,
    last_logged: Option<Instant>,
    now: Instant,
) -> LogAction {
    if next != prev {
        LogAction::Changed
    } else if next >= Level::High
        && last_logged.is_none_or(|t| now.saturating_duration_since(t) >= REMINDER_EVERY)
    {
        LogAction::Reminder
    } else {
        LogAction::Quiet
    }
}

/// The spool's view of the disk level: the monitor writes it, the spool
/// reads it before it admits a new reservation.
#[derive(Debug, Default)]
pub struct SpoolGate {
    level: AtomicU8,
    message: Mutex<String>,
}

impl SpoolGate {
    pub fn level(&self) -> Level {
        Level::from_u8(self.level.load(Ordering::Acquire))
    }

    /// Set by the monitor. `message` is what a refused request reads.
    pub fn set(&self, level: Level, message: String) {
        *self.message.lock() = message;
        self.level.store(level.as_u8(), Ordering::Release);
    }

    /// `Some(message)` while the spool refuses new reservations.
    pub fn refusal(&self) -> Option<String> {
        (self.level() >= Level::High).then(|| self.message.lock().clone())
    }
}

/// One measured path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathFacts {
    pub free: u64,
    pub total: u64,
    /// The device id: a device is judged once, whatever roles it holds.
    pub device: u64,
}

/// Where the monitor gets its numbers. The system implementation reads
/// `statvfs`; a test injects its own.
pub trait DiskProbe: Send + Sync {
    fn measure(&self, path: &Path) -> Option<PathFacts>;
}

/// `statvfs`, or the debug seam `DGP_TEST_DISK_MONITOR_FREE_PERCENT`.
pub struct SystemProbe;

impl DiskProbe for SystemProbe {
    fn measure(&self, path: &Path) -> Option<PathFacts> {
        if let Some(percent) = crate::config::test_seams::test_seams().disk_monitor_free_percent {
            let total = 100 * 1024 * 1024 * 1024u64;
            return Some(PathFacts {
                free: percent_of(total, percent),
                total,
                device: 0,
            });
        }
        crate::config::limits::measure_path(path).map(|(free, total, device)| PathFacts {
            free,
            total,
            device,
        })
    }
}

/// The paths the monitor watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Targets {
    pub spool_dir: PathBuf,
    /// Filesystem backends, by name.
    pub backends: Vec<(String, PathBuf)>,
    pub data_dir: PathBuf,
    /// Where a bucket with no route goes.
    pub default_backend: String,
    /// Also judge the spool by its use against its budget ([`SpoolView`]).
    pub spool_by_use: bool,
}

/// What the spool is sized to, for the thresholds and the gauges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sizing {
    pub budget: u64,
    pub used: u64,
    pub largest_reservation: u64,
}

/// One role at the end of a tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleReport {
    /// `spool`, `backend:<name>` or `data`.
    pub label: String,
    pub path: PathBuf,
    pub facts: Facts,
    pub level: Level,
    /// Roles with the same device share one level.
    pub device: u64,
}

/// The role of the spool judged by its use, and its stand-in device id (a
/// real device id never has every bit set).
pub const SPOOL_BUDGET_ROLE: &str = "spool-budget";
const USE_DEVICE: u64 = u64::MAX;

/// How to judge the spool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoolView {
    /// The free space of the disk says it all.
    Disk,
    /// The free space may hide the real limit: judge the use too.
    DiskAndUse(UseReason),
}

/// Why the free space of the spool directory is not the whole truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseReason {
    /// On a tmpfs the free space is memory.
    Tmpfs,
    /// Under Kubernetes an `emptyDir` `sizeLimit` is not in the free space.
    Kubernetes,
    /// The free space could not be measured.
    Unmeasured,
}

impl UseReason {
    pub fn describe(self) -> &'static str {
        match self {
            UseReason::Tmpfs => "the directory is on a tmpfs, so its free space is memory",
            UseReason::Kubernetes => {
                "the proxy runs under Kubernetes, where the free space does not show an \
                 emptyDir sizeLimit or a quota"
            }
            UseReason::Unmeasured => "the free space could not be measured",
        }
    }
}

/// Pure: how the monitor judges the spool, from what the boot found.
pub fn spool_view(is_tmpfs: bool, in_kubernetes: bool, measured: bool) -> SpoolView {
    if !measured {
        SpoolView::DiskAndUse(UseReason::Unmeasured)
    } else if is_tmpfs {
        SpoolView::DiskAndUse(UseReason::Tmpfs)
    } else if in_kubernetes {
        SpoolView::DiskAndUse(UseReason::Kubernetes)
    } else {
        SpoolView::Disk
    }
}

/// The verdict of the boot check of the spool directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootSpaceVerdict {
    /// The directory has at least a budget of free space.
    Fits,
    /// The budget is above the free space: uploads can fill the disk
    /// before the budget is full.
    BudgetAboveFree { budget: u64, free: u64 },
    /// The free space is no proof either way ([`SpoolView`]).
    NotVisible(UseReason),
}

/// Pure boot check: is there room for a full spool budget? `free` is the
/// free space of the directory plus the spool files the proxy can reuse
/// (`None`: not measured).
pub fn boot_space_verdict(view: SpoolView, free: Option<u64>, budget: u64) -> BootSpaceVerdict {
    match (view, free) {
        (SpoolView::DiskAndUse(reason), _) => BootSpaceVerdict::NotVisible(reason),
        (SpoolView::Disk, None) => BootSpaceVerdict::NotVisible(UseReason::Unmeasured),
        (SpoolView::Disk, Some(free)) if free < budget => {
            BootSpaceVerdict::BudgetAboveFree { budget, free }
        }
        (SpoolView::Disk, Some(_)) => BootSpaceVerdict::Fits,
    }
}

/// The boot warning of a verdict, or `None`. `dir` is the spool directory.
pub fn boot_space_message(verdict: BootSpaceVerdict, dir: &Path) -> Option<String> {
    match verdict {
        BootSpaceVerdict::Fits => None,
        BootSpaceVerdict::BudgetAboveFree { budget, free } => Some(format!(
            "the spool directory {} has {} free, below the spool budget DGP_SPOOL_MAX_BYTES of \
             {}. Uploads can fill the disk before the budget is full, and then a running write \
             fails with ENOSPC. Lower DGP_SPOOL_MAX_BYTES, free disk space, or move DGP_SPOOL_DIR \
             to a larger disk.",
            dir.display(),
            human(free),
            human(budget)
        )),
        BootSpaceVerdict::NotVisible(reason) => Some(format!(
            "the disk monitor cannot trust the free space of the spool directory {}: {}. It \
             judges the spool by its use against DGP_SPOOL_MAX_BYTES instead; set that value to \
             the space that the directory really has.",
            dir.display(),
            reason.describe()
        )),
    }
}

/// Pure: the boot warning, or `None` when the boot has nothing to say or the
/// derivation of the limits already warned about the same thing (it warns
/// about an explicit budget near the free space, about a floor above it and
/// about a free space that is no size).
pub fn boot_check(
    view: SpoolView,
    free: Option<u64>,
    budget: u64,
    dir: &Path,
    limit_warnings: &[crate::config::limits::LimitWarning],
) -> Option<String> {
    use crate::config::limits::LimitWarning as W;
    let verdict = boot_space_verdict(view, free, budget);
    let said_already = limit_warnings.iter().any(|w| match verdict {
        BootSpaceVerdict::Fits => false,
        BootSpaceVerdict::BudgetAboveFree { .. } => {
            matches!(
                w,
                W::SpoolAboveFreeSpace { .. } | W::FloorAboveAvailable { .. }
            )
        }
        BootSpaceVerdict::NotVisible(_) => matches!(w, W::FreeSpaceUnreliable { .. }),
    });
    if said_already {
        return None;
    }
    boot_space_message(verdict, dir)
}

#[derive(Default)]
struct DeviceState {
    level: Option<Level>,
    last_logged: Option<Instant>,
}

#[derive(Default)]
struct State {
    devices: HashMap<u64, DeviceState>,
    /// The last report of the spool DISK. A tick that cannot measure the
    /// disk judges the spool by it again (a failed measure changes nothing).
    spool_disk_last: Option<RoleReport>,
    gauge_labels: HashSet<String>,
}

#[derive(Default)]
struct Refusals {
    default_backend: String,
    /// Backends whose device is critical, with the message of the refusal.
    critical: BTreeMap<String, String>,
}

/// The monitor's state.
pub struct DiskWatch {
    spool_gate: Arc<SpoolGate>,
    state: Mutex<State>,
    refusals: RwLock<Refusals>,
}

static INSTALLED: OnceLock<Arc<DiskWatch>> = OnceLock::new();

/// The monitor of this process, once the boot installed it.
pub fn installed() -> Option<&'static Arc<DiskWatch>> {
    INSTALLED.get()
}

fn human(n: u64) -> String {
    crate::config::limits::human_bytes(n)
}

/// Pure: the message of a refused new spool reservation.
pub fn spool_refusal_message(level: Level, label: &str, path: &Path, facts: Facts) -> String {
    let what = if label == SPOOL_BUDGET_ROLE {
        format!(
            "the spool budget (DGP_SPOOL_MAX_BYTES) of the directory {} is nearly all reserved, \
             at level {} ({} of {} left)",
            path.display(),
            level.name(),
            human(facts.free),
            human(facts.total)
        )
    } else {
        format!(
            "the disk of the spool directory {} is at level {} ({} free of {})",
            path.display(),
            level.name(),
            human(facts.free),
            human(facts.total)
        )
    };
    format!(
        "{what}; the proxy refuses new uploads that need spool space until there is more. \
         Running requests, reads and deletes are not affected. Free disk space, lower \
         DGP_SPOOL_MAX_BYTES, or move DGP_SPOOL_DIR to a larger disk; retry later"
    )
}

/// Pure: the message of a refused write to a bucket of a filesystem backend.
pub fn backend_refusal_message(backend: &str, path: &Path, facts: Facts) -> String {
    format!(
        "the disk of backend '{backend}' ({}) is at level critical ({} free of {}); the proxy \
         refuses writes to its buckets until the disk has more free space. Reads and deletes still \
         work. Delete objects or add disk space, then retry",
        path.display(),
        human(facts.free),
        human(facts.total)
    )
}

/// Pure: the log line of a level change or a reminder.
pub fn log_line(level: Level, roles: &[&RoleReport], reminder: bool) -> String {
    let places = roles
        .iter()
        .map(|r| {
            format!(
                "{} {} ({} free of {})",
                r.label,
                r.path.display(),
                human(r.facts.free),
                human(r.facts.total)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let advice = match level {
        Level::Ok => "the disk has recovered",
        Level::Low => {
            "free disk space, or lower DGP_SPOOL_MAX_BYTES or point DGP_SPOOL_DIR to a larger disk"
        }
        Level::High => {
            "the spool refuses new reservations with a retryable 503 until the disk recovers; \
             free disk space or lower DGP_SPOOL_MAX_BYTES"
        }
        Level::Critical => {
            "the spool refuses new reservations, and writes to the buckets of a filesystem backend \
             on this disk get a retryable 503; free disk space now, or lower DGP_SPOOL_MAX_BYTES"
        }
    };
    format!(
        "[disk] level {}{}: {places}; {advice}",
        level.name(),
        if reminder { " (still)" } else { "" }
    )
}

impl DiskWatch {
    pub fn new(spool_gate: Arc<SpoolGate>) -> Self {
        Self {
            spool_gate,
            state: Mutex::new(State::default()),
            refusals: RwLock::new(Refusals::default()),
        }
    }

    /// Make the monitor of this process (once). The boot calls it before it
    /// spawns the monitor loop.
    pub fn install(spool_gate: Arc<SpoolGate>) -> Arc<DiskWatch> {
        INSTALLED
            .get_or_init(|| Arc::new(DiskWatch::new(spool_gate)))
            .clone()
    }

    pub fn spool_level(&self) -> Level {
        self.spool_gate.level()
    }

    /// The refusal of a client write to a bucket on `backend` (`None`: the
    /// bucket has no route, so it goes to the default backend).
    pub fn write_refusal(&self, backend: Option<&str>) -> Option<String> {
        let refusals = self.refusals.read();
        if refusals.critical.is_empty() {
            return None;
        }
        let name = backend.unwrap_or(refusals.default_backend.as_str());
        refusals.critical.get(name).cloned()
    }

    /// Is any filesystem backend at level critical?
    pub fn any_backend_critical(&self) -> bool {
        !self.refusals.read().critical.is_empty()
    }

    /// Measure every role, move the level of every device, apply the
    /// effects, set the gauges and log. Returns the roles for a test.
    pub fn tick(
        &self,
        probe: &dyn DiskProbe,
        targets: &Targets,
        tuning: &DiskMonitorTuning,
        sizing: Sizing,
        now: Instant,
        metrics: Option<&Metrics>,
    ) -> Vec<RoleReport> {
        let th = Thresholds::new(tuning, sizing.largest_reservation);
        let th_use = Thresholds::percent_only(tuning);
        let mut roles: Vec<(String, PathBuf, Option<PathFacts>)> = Vec::new();
        let spool_facts = probe.measure(&targets.spool_dir);
        roles.push(("spool".into(), targets.spool_dir.clone(), spool_facts));
        if targets.spool_by_use || spool_facts.is_none() {
            // The use of the budget as a measure: free = what is not reserved.
            roles.push((
                SPOOL_BUDGET_ROLE.into(),
                targets.spool_dir.clone(),
                Some(PathFacts {
                    free: sizing.budget.saturating_sub(sizing.used),
                    total: sizing.budget,
                    device: USE_DEVICE,
                }),
            ));
        }
        for (name, path) in &targets.backends {
            roles.push((format!("backend:{name}"), path.clone(), probe.measure(path)));
        }
        roles.push((
            "data".into(),
            targets.data_dir.clone(),
            probe.measure(&targets.data_dir),
        ));

        let mut state = self.state.lock();
        // One level per device.
        let mut device_levels: BTreeMap<u64, (Level, Level, Facts)> = BTreeMap::new();
        for (_, _, facts) in &roles {
            let Some(pf) = facts else { continue };
            if device_levels.contains_key(&pf.device) {
                continue;
            }
            let f = Facts {
                free: pf.free,
                total: pf.total,
            };
            let prev = state
                .devices
                .get(&pf.device)
                .and_then(|d| d.level)
                .unwrap_or(Level::Ok);
            let rule = if pf.device == USE_DEVICE {
                &th_use
            } else {
                &th
            };
            device_levels.insert(pf.device, (prev, next_level(prev, f, rule), f));
        }

        let mut reports = Vec::new();
        for (label, path, facts) in &roles {
            let Some(pf) = facts else { continue };
            let (_, level, f) = device_levels[&pf.device];
            reports.push(RoleReport {
                label: label.clone(),
                path: path.clone(),
                facts: f,
                level,
                device: pf.device,
            });
        }

        // Log: once per device on a change, a reminder while high or worse.
        for (device, (prev, next, _)) in &device_levels {
            let entry = state.devices.entry(*device).or_default();
            let action = log_action(*prev, *next, entry.last_logged, now);
            entry.level = Some(*next);
            if action == LogAction::Quiet {
                continue;
            }
            entry.last_logged = Some(now);
            let on_device: Vec<&RoleReport> =
                reports.iter().filter(|r| r.device == *device).collect();
            let line = log_line(*next, &on_device, action == LogAction::Reminder);
            match *next {
                Level::Ok => tracing::info!("{line}"),
                Level::Low | Level::High => tracing::warn!("{line}"),
                Level::Critical => tracing::error!("{line}"),
            }
        }

        // Effects.
        // The spool level is the worse of its disk and its use. A disk that
        // could not be measured now keeps its last verdict.
        if let Some(disk) = reports.iter().find(|r| r.label == "spool") {
            state.spool_disk_last = Some(disk.clone());
        }
        let spool_disk = state.spool_disk_last.clone();
        if let Some(spool) = reports
            .iter()
            .filter(|r| r.label == SPOOL_BUDGET_ROLE)
            .chain(spool_disk.as_ref())
            .max_by_key(|r| r.level)
        {
            self.spool_gate.set(
                spool.level,
                spool_refusal_message(spool.level, &spool.label, &spool.path, spool.facts),
            );
        }
        {
            let mut refusals = self.refusals.write();
            refusals.default_backend = targets.default_backend.clone();
            refusals.critical.clear();
            for r in &reports {
                if let Some(name) = r.label.strip_prefix("backend:") {
                    if r.level == Level::Critical {
                        refusals.critical.insert(
                            name.to_string(),
                            backend_refusal_message(name, &r.path, r.facts),
                        );
                    }
                }
            }
        }

        if let Some(m) = metrics {
            m.spool_budget_bytes.set(clamp_i64(sizing.budget));
            m.spool_used_bytes.set(clamp_i64(sizing.used));
            let now_labels: HashSet<String> = reports.iter().map(|r| r.label.clone()).collect();
            for gone in state.gauge_labels.difference(&now_labels) {
                let _ = m.disk_free_bytes.remove_label_values(&[gone]);
                let _ = m.disk_total_bytes.remove_label_values(&[gone]);
                let _ = m.disk_level.remove_label_values(&[gone]);
            }
            for r in &reports {
                match r.label.as_str() {
                    "spool" => {
                        m.spool_dir_free_bytes.set(clamp_i64(r.facts.free));
                        m.spool_dir_total_bytes.set(clamp_i64(r.facts.total));
                    }
                    // The use of the budget has its own gauges above.
                    SPOOL_BUDGET_ROLE => {}
                    _ => {
                        m.disk_free_bytes
                            .with_label_values(&[&r.label])
                            .set(clamp_i64(r.facts.free));
                        m.disk_total_bytes
                            .with_label_values(&[&r.label])
                            .set(clamp_i64(r.facts.total));
                    }
                }
                m.disk_level
                    .with_label_values(&[&r.label])
                    .set(i64::from(r.level.as_u8()));
            }
            state.gauge_labels = now_labels;
        }
        reports
    }
}

/// THE disk refusal of a write to `bucket`: `Some(message)` while the disk
/// of the filesystem backend that holds the bucket is at level critical.
/// The client-store gate (`check_client_store_allowed`) turns it into
/// `503 SlowDown`; the background writers (replication, lifecycle) defer on
/// it through the maintenance gate. Costs one lock read while no backend
/// is critical.
pub fn bucket_write_refusal(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> Option<String> {
    use crate::storage::StorageBackend as _;
    let watch = installed()?;
    if !watch.any_backend_critical() {
        return None;
    }
    watch.write_refusal(engine.storage().resolved_backend_name(bucket).as_deref())
}

fn clamp_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// How the spool is judged under this config (the boot facts).
pub fn spool_view_of(config: &crate::config::Config) -> SpoolView {
    config.tuning.limits.as_ref().map_or(SpoolView::Disk, |l| {
        spool_view(l.facts.is_tmpfs, l.facts.in_kubernetes, l.facts.measured())
    })
}

/// The boot check of the running process: measures the spool directory now.
pub fn boot_check_now(
    config: &crate::config::Config,
    spool: &crate::deltaglider::spool::SpoolDir,
) -> Option<String> {
    let own = config
        .tuning
        .limits
        .as_ref()
        .map_or(0, |l| l.facts.own_spool_bytes);
    let free = SystemProbe
        .measure(spool.dir())
        .map(|f| f.free.saturating_add(own));
    let warnings = config
        .tuning
        .limits
        .as_ref()
        .map_or(&[][..], |l| l.warnings.as_slice());
    boot_check(
        spool_view_of(config),
        free,
        spool.max_bytes(),
        spool.dir(),
        warnings,
    )
}

/// One tick for the running process: the paths and the sizes come from the
/// engine and the config of now.
pub fn tick_app(
    watch: &DiskWatch,
    state: &crate::api::handlers::AppState,
    config: &crate::config::Config,
) {
    let engine = state.engine.load();
    let spool = engine.spool_dir();
    let backends = config
        .effective_backends()
        .into_iter()
        .filter_map(|(name, b)| match b {
            crate::config::BackendConfig::Filesystem { path } => {
                Some((name.to_string(), path.clone()))
            }
            _ => None,
        })
        .collect();
    let data_dir = crate::config_db::config_db_path()
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let targets = Targets {
        spool_dir: spool.dir().to_path_buf(),
        backends,
        data_dir,
        default_backend: config.default_backend_name(),
        spool_by_use: spool_view_of(config) != SpoolView::Disk,
    };
    let budget = spool.max_bytes();
    let per_upload = config
        .tuning
        .limits
        .as_ref()
        .map_or(0, |l| l.effective.per_upload);
    let largest = if per_upload == 0 {
        budget
    } else {
        per_upload.min(budget)
    };
    watch.tick(
        &SystemProbe,
        &targets,
        &config.tuning.disk_monitor,
        Sizing {
            budget,
            used: spool.used_bytes(),
            largest_reservation: largest,
        },
        Instant::now(),
        Some(&state.metrics),
    );
}

#[cfg(test)]
mod tests;
