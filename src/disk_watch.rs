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
//! The monitor also sets the gauges `deltaglider_spool_budget_bytes`,
//! `deltaglider_spool_used_bytes` and, per role (`spool`, `backend:<name>`,
//! `data`), `deltaglider_disk_free_bytes`, `deltaglider_disk_total_bytes`
//! and `deltaglider_disk_level`.

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

#[derive(Default)]
struct DeviceState {
    level: Option<Level>,
    last_logged: Option<Instant>,
}

#[derive(Default)]
struct State {
    devices: HashMap<u64, DeviceState>,
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
pub fn spool_refusal_message(level: Level, path: &Path, facts: Facts) -> String {
    format!(
        "the disk of the spool directory {} is at level {} ({} free of {}); the proxy refuses new \
         uploads that need spool space until the disk has more free space. Running requests, reads \
         and deletes are not affected. Free disk space, lower DGP_SPOOL_MAX_BYTES, or move \
         DGP_SPOOL_DIR to a larger disk; retry later",
        path.display(),
        level.name(),
        human(facts.free),
        human(facts.total)
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
            "the spool refuses new reservations with 503 SlowDown until the disk recovers; free \
             disk space"
        }
        Level::Critical => {
            "the spool refuses new reservations, and writes to the buckets of a filesystem backend \
             on this disk get 503 SlowDown; free disk space now"
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
        let mut roles: Vec<(String, PathBuf, Option<PathFacts>)> = Vec::new();
        roles.push((
            "spool".into(),
            targets.spool_dir.clone(),
            probe.measure(&targets.spool_dir),
        ));
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
            device_levels.insert(pf.device, (prev, next_level(prev, f, &th), f));
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
        if let Some(spool) = reports.iter().find(|r| r.label == "spool") {
            self.spool_gate.set(
                spool.level,
                spool_refusal_message(spool.level, &spool.path, spool.facts),
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
                m.disk_free_bytes
                    .with_label_values(&[&r.label])
                    .set(clamp_i64(r.facts.free));
                m.disk_total_bytes
                    .with_label_values(&[&r.label])
                    .set(clamp_i64(r.facts.total));
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
    engine: &crate::deltaglider::engine::DynEngine,
    bucket: &str,
) -> Option<String> {
    let watch = installed()?;
    if !watch.any_backend_critical() {
        return None;
    }
    watch.write_refusal(engine.storage().resolved_backend_name(bucket).as_deref())
}

fn clamp_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
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
