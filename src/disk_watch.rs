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
//! * critical on the device of a filesystem backend: the writes that STORE
//!   bytes into the buckets of that backend get `503 SlowDown`
//!   ([`DiskWatch::write_refusal_for`], asked by the client-store gate and
//!   by the maintenance gate for a copy). A delete and a read never ask.
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
}

/// The free and the total bytes of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    pub free: u64,
    pub total: u64,
}

/// The thresholds of the levels. A level starts when the free space is
/// below `min(max(percent of the disk, floor), ceiling)`:
/// * the percent follows the size of the disk;
/// * the floor (bytes taken from the spool limits) keeps a small disk from
///   running out under one upload;
/// * the ceiling (bytes) keeps a huge disk from refusing writes while it
///   still has plenty of room: 5% of a 10 TiB volume is 512 GiB.
///
/// A ceiling of `0` turns the level off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub low_percent: u8,
    pub high_percent: u8,
    pub critical_percent: u8,
    /// A level ends when the free space is this many percentage points of
    /// the disk above its entry threshold (never more than a fifth of the
    /// OWN ceiling of the level that ends).
    pub recover_points: u8,
    /// Twice the largest reservation that the spool allows for one upload.
    pub low_floor: u64,
    /// The largest reservation that the spool allows for one upload.
    pub high_floor: u64,
    pub critical_floor: u64,
    /// The most free space at which level low starts (`0`: no level low).
    pub low_max: u64,
    pub high_max: u64,
    pub critical_max: u64,
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
            low_max: tuning.low_max_bytes,
            high_max: tuning.high_max_bytes,
            critical_max: tuning.critical_max_bytes,
        }
        .monotone()
    }

    /// The thresholds for a measure that is no disk: the use of the spool
    /// against its budget. The budget already bounds every reservation, so
    /// neither a floor nor a ceiling in bytes applies, only the percents. A
    /// level that the tuning turns off stays off.
    pub fn percent_only(tuning: &DiskMonitorTuning) -> Self {
        let on = |max: u64| if max == 0 { 0 } else { u64::MAX };
        Self {
            low_floor: 0,
            high_floor: 0,
            critical_floor: 0,
            low_max: on(tuning.low_max_bytes),
            high_max: on(tuning.high_max_bytes),
            critical_max: on(tuning.critical_max_bytes),
            ..Self::new(tuning, 0)
        }
    }

    /// Ceilings that never fall for a lower level: a settings mistake (a
    /// higher level with the larger ceiling) cannot make the levels
    /// overlap. An off level (`0`) is skipped.
    pub fn monotone(mut self) -> Self {
        if self.critical_max != 0 && self.high_max != 0 {
            self.high_max = self.high_max.max(self.critical_max);
        }
        let above = if self.high_max != 0 {
            self.high_max
        } else {
            self.critical_max
        };
        if self.low_max != 0 && above != 0 {
            self.low_max = self.low_max.max(above);
        }
        self
    }

    fn ceiling(&self, level: Level) -> u64 {
        match level {
            Level::Ok => 0,
            Level::Low => self.low_max,
            Level::High => self.high_max,
            Level::Critical => self.critical_max,
        }
    }

    /// Does this level exist (`Ok` always does)?
    pub fn enabled(&self, level: Level) -> bool {
        level == Level::Ok || self.ceiling(level) != 0
    }

    /// The free space below which `level` starts: the larger of the
    /// thresholds of every level above it, so the levels never overlap.
    /// `Ok` and a level that is off have none (0).
    pub fn enter(&self, level: Level, total: u64) -> u64 {
        let own = |l: Level| {
            let (percent, floor) = match l {
                Level::Ok => return 0,
                Level::Low => (self.low_percent, self.low_floor),
                Level::High => (self.high_percent, self.high_floor),
                Level::Critical => (self.critical_percent, self.critical_floor),
            };
            if !self.enabled(l) {
                return 0;
            }
            percent_of(total, percent).max(floor).min(self.ceiling(l))
        };
        if !self.enabled(level) {
            return 0;
        }
        let mut value = own(level);
        for above in [Level::High, Level::Critical] {
            if above > level {
                value = value.max(own(above));
            }
        }
        value
    }

    /// The bytes above the entry threshold at which `level` ends: the
    /// percent points of the disk, and at most a fifth of the level's own ceiling (so
    /// a huge disk does not wait for hundreds of GiB to recover).
    pub fn recover_margin(&self, level: Level, total: u64) -> u64 {
        percent_of(total, self.recover_points).min(self.ceiling(level) / 5)
    }

    /// The level below `level` that exists (`Ok` at the end).
    pub fn lower(&self, level: Level) -> Level {
        let mut l = level;
        while l > Level::Ok {
            l = Level::from_u8(l.as_u8() - 1);
            if self.enabled(l) {
                return l;
            }
        }
        Level::Ok
    }
}

/// The level that the free space says, with no memory of the past.
pub fn raw_level(facts: Facts, th: &Thresholds) -> Level {
    Level::ALL
        .into_iter()
        .rev()
        .find(|l| *l != Level::Ok && th.enabled(*l) && facts.free < th.enter(*l, facts.total))
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
        .saturating_add(th.recover_margin(prev, facts.total));
    if facts.free >= leave_at {
        th.lower(prev)
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

/// The spool's view of the disk: the monitor writes it, the spool reads it
/// before it admits a new reservation.
///
/// Two things refuse a NEW write. The level of the spool DISK is the
/// monitor's last verdict ([`SpoolGate::set`]). The USE of the budget, where
/// the free space may hide the real limit ([`SpoolView`]), is judged live
/// at every reservation from `used / budget` ([`use_refusal`]): it moves in
/// an instant, and a verdict that is 30 s old would keep refusing after a
/// spike ended.
#[derive(Debug, Default)]
pub struct SpoolGate {
    level: AtomicU8,
    message: Mutex<String>,
    by_use: Mutex<Option<Thresholds>>,
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

    /// `Some(message)` while the disk of the spool is at level high or
    /// critical.
    pub fn refusal(&self) -> Option<String> {
        (self.level() >= Level::High).then(|| self.message.lock().clone())
    }

    /// Turn the live judgement of the use of the budget on (with its
    /// thresholds) or off. Set by the monitor.
    pub fn set_by_use(&self, thresholds: Option<Thresholds>) {
        *self.by_use.lock() = thresholds;
    }

    /// The thresholds of the live judgement, when it is on.
    pub fn by_use(&self) -> Option<Thresholds> {
        *self.by_use.lock()
    }
}

/// Pure: the refusal of a new spool reservation by the use of the budget:
/// `Some(message)` when the reserved bytes (`used`) leave less than the high
/// threshold of the budget.
pub fn use_refusal(th: &Thresholds, budget: u64, used: u64, dir: &Path) -> Option<String> {
    let facts = Facts {
        free: budget.saturating_sub(used),
        total: budget,
    };
    let level = raw_level(facts, th);
    (level >= Level::High).then(|| spool_refusal_message(level, SPOOL_BUDGET_ROLE, dir, facts))
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
        let seams = crate::config::test_seams::test_seams();
        // The file seam is read at every measure: a test changes the level
        // of the disk while the proxy runs.
        let from_file = seams
            .disk_monitor_free_file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| t.trim().parse::<u8>().ok())
            .filter(|p| *p <= 100);
        if let Some(percent) = from_file.or(seams.disk_monitor_free_percent) {
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
    explicit: bool,
    dir: &Path,
    limit_warnings: &[crate::config::limits::LimitWarning],
) -> Option<String> {
    use crate::config::limits::LimitWarning as W;
    let verdict = boot_space_verdict(view, free, budget);
    // An explicit budget on a directory whose free space is no proof is the
    // right setup there: the warning is about a budget that nobody set.
    if explicit && matches!(verdict, BootSpaceVerdict::NotVisible(_)) {
        return None;
    }
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
    /// The last report of every configured role. A role whose measure
    /// fails keeps it: a failed measure changes nothing (not the level, not
    /// the refusal, not the series).
    last: HashMap<String, RoleReport>,
    gauge_labels: HashSet<String>,
    /// Whether the spool was judged by its use at the last tick that knew:
    /// a measure that fails or times out changes nothing.
    by_use: Option<bool>,
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
    /// The lines that the monitor logged (tests read them).
    #[cfg(test)]
    emitted: Mutex<Vec<(Level, String)>>,
}

static INSTALLED: OnceLock<Arc<DiskWatch>> = OnceLock::new();

/// The monitor of this process, once the boot installed it. A test installs
/// its own for its thread (`install_for_this_test`).
pub fn installed() -> Option<Arc<DiskWatch>> {
    #[cfg(test)]
    {
        if let Some(w) = TEST_WATCH.with(|w| w.borrow().clone()) {
            return Some(w);
        }
    }
    INSTALLED.get().cloned()
}

#[cfg(test)]
thread_local! {
    static TEST_WATCH: std::cell::RefCell<Option<Arc<DiskWatch>>> =
        const { std::cell::RefCell::new(None) };
}

/// A monitor for the tests of this thread only (a `current_thread` test
/// runtime runs its tasks on the test thread): the process-wide monitor, and
/// the other tests, never feel a test's full disk.
#[cfg(test)]
pub(crate) fn install_for_this_test(watch: Arc<DiskWatch>) {
    TEST_WATCH.with(|w| *w.borrow_mut() = Some(watch));
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

/// Pure: what to do about a level on a role. The text follows the role: the
/// spool refuses new uploads, a backend refuses writes only at critical, the
/// data directory refuses nothing.
pub fn advice(level: Level, label: &str) -> String {
    let spool = label == "spool" || label == SPOOL_BUDGET_ROLE;
    match (level, spool, label.strip_prefix("backend:")) {
        (Level::Ok, _, _) => "the disk has recovered".to_string(),
        (Level::Low, true, _) => {
            "free disk space, lower DGP_SPOOL_MAX_BYTES or point DGP_SPOOL_DIR to a larger disk"
                .to_string()
        }
        (Level::Low | Level::High, false, Some(name)) => format!(
            "no write is refused yet; at level critical the buckets of backend '{name}' refuse \
             writes. Delete objects or add disk space"
        ),
        (Level::Low | Level::High, false, None) => {
            "no write is refused. The config database and the job state live here: free disk space"
                .to_string()
        }
        (Level::High | Level::Critical, true, _) => {
            "the spool refuses new uploads that need spool space with a retryable 503 until the \
             disk recovers: free disk space or lower DGP_SPOOL_MAX_BYTES"
                .to_string()
        }
        (Level::Critical, false, Some(name)) => format!(
            "writes to the buckets of backend '{name}' get a retryable 503; reads and deletes \
             still work. Delete objects or add disk space now"
        ),
        (Level::Critical, false, None) => {
            "the config database and the job state live here and can fail to write: free disk \
             space now"
                .to_string()
        }
    }
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
    let mut seen: Vec<String> = Vec::new();
    for r in roles {
        let text = format!("{}: {}", r.label, advice(level, &r.label));
        if !seen.contains(&text) {
            seen.push(text);
        }
    }
    format!(
        "[disk] level {}{}: {places}; {}",
        level.name(),
        if reminder { " (still)" } else { "" },
        seen.join("; ")
    )
}

/// Pure: the refusal of a write into a bucket whose home is `home`, given
/// the backends at level critical. A bucket whose home the router cannot
/// tell is NOT judged by the default backend (that would be a guess): the
/// write meets its backend's own error.
pub fn refusal_for_home(
    critical: &BTreeMap<String, String>,
    default_backend: &str,
    home: &crate::storage::BucketHome,
) -> Option<String> {
    use crate::storage::BucketHome;
    match home {
        BucketHome::Backend(name) => critical.get(name).cloned(),
        BucketHome::Default => critical.get(default_backend).cloned(),
        BucketHome::Undetermined => None,
    }
}

impl DiskWatch {
    pub fn new(spool_gate: Arc<SpoolGate>) -> Self {
        Self {
            spool_gate,
            state: Mutex::new(State::default()),
            refusals: RwLock::new(Refusals::default()),
            #[cfg(test)]
            emitted: Mutex::new(Vec::new()),
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

    /// The refusal of a client write to a bucket whose home is `home`.
    pub fn write_refusal_for(&self, home: &crate::storage::BucketHome) -> Option<String> {
        let refusals = self.refusals.read();
        if refusals.critical.is_empty() {
            return None;
        }
        refusal_for_home(&refusals.critical, &refusals.default_backend, home)
    }

    /// Is any filesystem backend at level critical?
    pub fn any_backend_critical(&self) -> bool {
        !self.refusals.read().critical.is_empty()
    }

    /// Measure every role, move the level of every device, apply the
    /// effects, set the gauges and log. Returns the roles that were
    /// measured, for a test.
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
        // The spool is judged by its use too where the free space may hide
        // the real limit, and where it was never measured. A measure that
        // fails or times out (a hung mount) changes nothing: the last
        // decision stands, as the last level does.
        let by_use = {
            let state = self.state.lock();
            match (spool_facts, state.by_use) {
                (Some(_), _) => targets.spool_by_use,
                (None, Some(last)) => last,
                (None, None) => true,
            }
        };
        if by_use {
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
        // Every configured role has a verdict: this tick's, else the last.
        let configured: HashSet<String> = roles.iter().map(|(l, _, _)| l.clone()).collect();
        let mut effective = reports.clone();
        for label in &configured {
            if !reports.iter().any(|r| &r.label == label) {
                if let Some(last) = state.last.get(label) {
                    effective.push(last.clone());
                }
            }
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
            #[cfg(test)]
            self.emitted.lock().push((*next, line.clone()));
            match *next {
                Level::Ok => tracing::info!("{line}"),
                Level::Low | Level::High => tracing::warn!("{line}"),
                Level::Critical => tracing::error!("{line}"),
            }
        }

        // Effects. The spool gate follows the spool DISK; the use of the
        // budget is judged live, at every new reservation, from the same
        // thresholds ([`SpoolGate::set_by_use`]): a spike of use must not
        // keep refusing until the next tick.
        if let Some(disk) = effective.iter().find(|r| r.label == "spool") {
            self.spool_gate.set(
                disk.level,
                spool_refusal_message(disk.level, &disk.label, &disk.path, disk.facts),
            );
        }
        self.spool_gate.set_by_use(by_use.then_some(th_use));
        state.by_use = Some(by_use);
        {
            let mut refusals = self.refusals.write();
            refusals.default_backend = targets.default_backend.clone();
            refusals.critical.clear();
            for r in &effective {
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
            // A series goes only with its role, not with one failed measure.
            for gone in state.gauge_labels.difference(&configured) {
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
            state.gauge_labels = configured.clone();
        }
        state.last = effective
            .into_iter()
            .filter(|r| configured.contains(&r.label))
            .map(|r| (r.label.clone(), r))
            .collect();
        reports
    }
}

/// THE disk refusal of a write that STORES bytes into `bucket`:
/// `Some(message)` while the disk of the filesystem backend that holds the
/// bucket is at level critical. The router says where the bucket lives
/// (`bucket_home`: a route, a cached resolution, or the one HeadBucket round
/// that the write itself sends); a bucket whose home it cannot tell is not
/// refused (see [`refusal_for_home`]). Costs one lock read while no backend is
/// critical. A delete never asks: it frees space.
pub async fn bucket_write_refusal(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> Option<String> {
    let watch = installed()?;
    bucket_write_refusal_with(&watch, engine, bucket).await
}

/// [`bucket_write_refusal`] with the monitor given.
pub async fn bucket_write_refusal_with(
    watch: &DiskWatch,
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> Option<String> {
    use crate::storage::StorageBackend as _;
    if !watch.any_backend_critical() {
        return None;
    }
    let home = engine.storage().bucket_home(bucket).await;
    watch.write_refusal_for(&home)
}

/// [`bucket_write_refusal`] from memory only, for the synchronous gates of
/// the background writers: a bucket whose home the router does not know
/// without a request is not refused now (the router resolves it in the
/// background, so the next check knows).
pub fn bucket_write_refusal_now(
    watch: &DiskWatch,
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
) -> Option<String> {
    use crate::storage::StorageBackend as _;
    if !watch.any_backend_critical() {
        return None;
    }
    watch.write_refusal_for(&engine.storage().bucket_home_now(bucket))
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
    probe: &dyn DiskProbe,
    config: &crate::config::Config,
    spool: &crate::deltaglider::spool::SpoolDir,
) -> Option<String> {
    let own = config
        .tuning
        .limits
        .as_ref()
        .map_or(0, |l| l.facts.own_spool_bytes);
    let free = probe
        .measure(spool.dir())
        .map(|f| f.free.saturating_add(own));
    let warnings = config
        .tuning
        .limits
        .as_ref()
        .map_or(&[][..], |l| l.warnings.as_slice());
    let explicit = config
        .tuning
        .limits
        .as_ref()
        .is_some_and(|l| l.effective.spool_source == crate::config::limits::Source::Explicit);
    boot_check(
        spool_view_of(config),
        free,
        spool.max_bytes(),
        explicit,
        spool.dir(),
        warnings,
    )
}

/// One tick for the running process: the paths and the sizes come from the
/// engine and the config of now.
pub fn tick_app(
    watch: &DiskWatch,
    probe: &dyn DiskProbe,
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
        probe,
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

/// A probe with a deadline per measure. `statvfs` on a hung network mount
/// does not return; the thread that waits for it cannot be stopped. So each
/// measure runs on a thread of its own and the monitor waits `timeout` for
/// it. On a timeout the measure is `None` (the role keeps its last verdict,
/// [`DiskWatch::tick`]), and a path whose measure still hangs gets no second
/// thread until the first one returns. The timeout is logged once for a
/// path, until a measure of it succeeds again.
pub struct TimedProbe {
    inner: Arc<dyn DiskProbe>,
    timeout: Duration,
    /// Paths whose measure thread has not returned.
    hung: Arc<Mutex<HashSet<PathBuf>>>,
    /// Paths whose timeout is already logged.
    warned: Mutex<HashSet<PathBuf>>,
}

impl TimedProbe {
    pub fn new(inner: Arc<dyn DiskProbe>, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            hung: Arc::default(),
            warned: Mutex::default(),
        }
    }

    /// How many paths have a measure thread that has not returned.
    pub fn hung_paths(&self) -> usize {
        self.hung.lock().len()
    }
}

/// Removes a path from the hung set when the measure thread ends, however.
struct Unhung {
    hung: Arc<Mutex<HashSet<PathBuf>>>,
    path: PathBuf,
}

impl Drop for Unhung {
    fn drop(&mut self) {
        self.hung.lock().remove(&self.path);
    }
}

impl DiskProbe for TimedProbe {
    fn measure(&self, path: &Path) -> Option<PathFacts> {
        if !self.hung.lock().insert(path.to_path_buf()) {
            // The thread of an earlier measure of this path still waits.
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (inner, hung, owned) = (self.inner.clone(), self.hung.clone(), path.to_path_buf());
        let spawned = std::thread::Builder::new()
            .name("disk-measure".into())
            .spawn(move || {
                // Ends the hang on every exit of the thread, a panic of the
                // probe included: the path must never stay hung for good.
                let _unhung = Unhung {
                    hung,
                    path: owned.clone(),
                };
                let facts = inner.measure(&owned);
                let _ = tx.send(facts);
            });
        if spawned.is_err() {
            self.hung.lock().remove(path);
            return None;
        }
        match rx.recv_timeout(self.timeout) {
            Ok(facts) => {
                if facts.is_some() {
                    self.warned.lock().remove(path);
                }
                facts
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if self.warned.lock().insert(path.to_path_buf()) {
                    tracing::warn!(
                        "[disk] the free space of {} did not answer within {} s (a hung network \
                         mount?); its role keeps its last level until it answers. Set \
                         DGP_DISK_MEASURE_TIMEOUT_SECS to wait longer",
                        path.display(),
                        self.timeout.as_secs()
                    );
                }
                None
            }
            // The probe panicked: no answer, and the path is free again.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if self.warned.lock().insert(path.to_path_buf()) {
                    tracing::warn!(
                        "[disk] the free space of {} could not be measured (the measure \
                         panicked); its role keeps its last level",
                        path.display()
                    );
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
