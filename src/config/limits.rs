// SPDX-License-Identifier: BUSL-1.1

//! Auto-sized limits: the spool budget, the multipart limits and
//! `max_object_size`.
//!
//! The operator gives no value and the proxy derives one from the disk that
//! holds the spool directory. The rule is ONE pure function,
//! [`derive_limits`]: the facts of the disk ([`DiskFacts`]) and the values
//! that the operator set ([`ExplicitLimits`]) go in; the effective values
//! ([`EffectiveLimits`]) and the warnings come out. The disk is measured
//! once per process, at boot, after the spool sweep ([`measure_disk_facts`],
//! a thin wrapper around `statvfs`, `statfs` and `st_dev`). A limit never
//! changes under a running upload: the budget of a process is fixed, and a
//! config apply only derives the per-upload values again from it.

use super::EnvLookup;
use schemars::{gen::SchemaGenerator, schema::Schema, JsonSchema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// The least spool budget that `auto` and a percent give.
pub const SPOOL_FLOOR_BYTES: u64 = GIB;

/// The `max_object_size` that an auto value has before the disk is measured
/// (a unit test, `config lint`). It is the default of releases before auto
/// sizing.
pub const UNMEASURED_MAX_OBJECT_SIZE: u64 = 100 * MIB;

/// Share of the free space that `auto` gives to the spool.
pub const AUTO_SHARE_PERCENT: u64 = 50;

/// The same, when the spool shares its device with backend data.
pub const AUTO_SHARED_DEVICE_PERCENT: u64 = 25;

/// An explicit spool budget above this share of the free space warns.
pub const WARN_SHARE_PERCENT: u64 = 90;

/// `DGP_SPOOL_MAX_BYTES`: `auto`, a percent of the free space, or bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpoolSetting {
    #[default]
    Auto,
    /// 1 to 100.
    Percent(u8),
    Bytes(u64),
}

impl std::str::FromStr for SpoolSetting {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        if let Some(p) = s.strip_suffix('%') {
            return match p.trim().parse::<u8>() {
                Ok(p) if (1..=100).contains(&p) => Ok(Self::Percent(p)),
                _ => Err(format!(
                    "'{s}' is not a percent from 1% to 100%; use 'auto', a percent such as '40%', or a number of bytes"
                )),
            };
        }
        s.parse::<u64>().map(Self::Bytes).map_err(|_| {
            format!("'{s}' is not 'auto', a percent such as '40%', or a number of bytes")
        })
    }
}

impl std::fmt::Display for SpoolSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Percent(p) => write!(f, "{p}%"),
            Self::Bytes(n) => write!(f, "{n}"),
        }
    }
}

/// `max_object_size`: `auto` (the effective per-upload limit) or bytes.
/// In YAML: the word `auto` or a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BytesOrAuto {
    #[default]
    Auto,
    Bytes(u64),
}

impl std::str::FromStr for BytesOrAuto {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse::<u64>()
            .map(Self::Bytes)
            .map_err(|_| format!("'{s}' is not 'auto' or a number of bytes"))
    }
}

impl std::fmt::Display for BytesOrAuto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Bytes(n) => write!(f, "{n}"),
        }
    }
}

impl From<u64> for BytesOrAuto {
    fn from(n: u64) -> Self {
        Self::Bytes(n)
    }
}

impl Serialize for BytesOrAuto {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Bytes(n) => s.serialize_u64(*n),
        }
    }
}

impl<'de> Deserialize<'de> for BytesOrAuto {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = BytesOrAuto;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the word 'auto' or a number of bytes")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(BytesOrAuto::Bytes(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(BytesOrAuto::Bytes)
                    .map_err(|_| E::custom("a size cannot be negative"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                v.parse().map_err(E::custom)
            }
        }
        d.deserialize_any(V)
    }
}

impl JsonSchema for BytesOrAuto {
    fn schema_name() -> String {
        "BytesOrAuto".to_string()
    }
    fn json_schema(gen: &mut SchemaGenerator) -> Schema {
        use schemars::schema::{InstanceType, Metadata, SchemaObject, SubschemaValidation};
        let word = SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            enum_values: Some(vec![serde_json::json!("auto")]),
            ..Default::default()
        };
        SchemaObject {
            metadata: Some(Box::new(Metadata {
                description: Some(
                    "A number of bytes, or the word auto (derived from the disk at boot)."
                        .to_string(),
                ),
                ..Default::default()
            })),
            subschemas: Some(Box::new(SubschemaValidation {
                any_of: Some(vec![gen.subschema_for::<u64>(), word.into()]),
                ..Default::default()
            })),
            ..Default::default()
        }
        .into()
    }
}

/// What the disk of the spool directory looks like at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskFacts {
    /// Bytes that a new file can use (`f_bavail`).
    pub free: u64,
    /// Size of the filesystem. 0 = the measure failed.
    pub total: u64,
    /// Bytes of spool files already in the directory after the sweep (the
    /// proxy counts them as free: it can reuse them).
    pub own_spool_bytes: u64,
    /// The directory is on a tmpfs: its free space is RAM.
    pub is_tmpfs: bool,
    /// The directory is on the same device as a filesystem backend root or
    /// the data directory.
    pub shares_device_with_data: bool,
    /// The proxy runs under Kubernetes: an `emptyDir` `sizeLimit` is not
    /// visible in the free space.
    pub in_kubernetes: bool,
}

impl DiskFacts {
    /// Free space plus the spool files that the proxy can reuse.
    pub fn available(&self) -> u64 {
        self.free.saturating_add(self.own_spool_bytes)
    }
    pub fn measured(&self) -> bool {
        self.total > 0
    }
}

/// What the operator set. `None` = not set (auto).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExplicitLimits {
    pub spool: SpoolSetting,
    /// `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`; `Some(0)` = no cap.
    pub per_upload: Option<u64>,
    /// `DGP_MAX_TOTAL_MULTIPART_BYTES`.
    pub total_multipart: Option<u64>,
    pub max_object_size: BytesOrAuto,
}

/// Where a value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Auto,
    Explicit,
}

/// How the spool budget was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoolBasis {
    /// `percent` of `available` bytes (`auto`: 50 or 25; or the operator's
    /// percent).
    Share { percent: u64, available: u64 },
    /// The operator gave bytes.
    Bytes,
    /// The floor, because the free space is not a reliable size.
    Floor(FloorReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloorReason {
    Tmpfs,
    Kubernetes,
    Unmeasured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveLimits {
    pub spool_budget: u64,
    pub spool_source: Source,
    pub spool_basis: SpoolBasis,
    /// 0 = no cap.
    pub per_upload: u64,
    pub per_upload_source: Source,
    pub total_multipart: u64,
    pub total_multipart_source: Source,
    pub max_object_size: u64,
    pub max_object_size_source: Source,
}

/// The setting that a warning asks the operator to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadLimit {
    PerUpload,
    MaxObjectSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitWarning {
    /// `auto` could not use the free space as a size.
    FreeSpaceUnreliable { reason: FloorReason },
    /// The floor is above the space that the disk has.
    FloorAboveAvailable { available: u64 },
    /// An explicit budget is too close to, or above, the free space.
    SpoolAboveFreeSpace {
        budget: u64,
        available: u64,
        shared_device: bool,
    },
    /// An explicit upload limit is above half of the budget: an upload of
    /// that size cannot fit.
    UploadAboveHalfBudget {
        which: UploadLimit,
        value: u64,
        limit: u64,
        budget: u64,
    },
}

/// Round down to whole MiB.
fn round_down_mib(n: u64) -> u64 {
    n - n % MIB
}

/// `percent` % of `n`, without overflow.
fn percent_of(n: u64, percent: u64) -> u64 {
    (u128::from(n) * u128::from(percent) / 100) as u64
}

/// The default per-upload limit for a spool budget: half of it, at least
/// 1 MiB.
pub fn default_per_upload(spool_budget: u64) -> u64 {
    (spool_budget / 2).max(MIB)
}

/// The default total of all multipart uploads: half of the budget.
pub fn default_total_multipart(spool_budget: u64) -> u64 {
    spool_budget / 2
}

/// THE rule of the auto-sized limits. Pure.
///
/// Spool budget:
/// - bytes: as given (explicit always wins);
/// - `auto`: 50% of the free space plus the proxy's own spool files, 25%
///   when the directory shares a device with backend data; whole MiB, at
///   least 1 GiB. On a tmpfs, under Kubernetes, or when the measure failed,
///   the free space is not the real limit: the floor, and a warning;
/// - a percent: that share of the same space, whole MiB, at least 1 GiB.
///
/// The per-upload limit and the total of the multipart uploads are half of
/// the budget; `max_object_size: auto` is the per-upload limit.
pub fn derive_limits(
    facts: DiskFacts,
    explicit: ExplicitLimits,
) -> (EffectiveLimits, Vec<LimitWarning>) {
    let mut warnings = Vec::new();
    let available = facts.available();
    let share = |percent: u64| {
        let budget = round_down_mib(percent_of(available, percent)).max(SPOOL_FLOOR_BYTES);
        (budget, SpoolBasis::Share { percent, available })
    };
    let (spool_budget, spool_source, spool_basis) = match explicit.spool {
        SpoolSetting::Bytes(n) => (n, Source::Explicit, SpoolBasis::Bytes),
        SpoolSetting::Percent(p) => {
            let (b, basis) = share(u64::from(p));
            (b, Source::Explicit, basis)
        }
        SpoolSetting::Auto => {
            let reason = if !facts.measured() {
                Some(FloorReason::Unmeasured)
            } else if facts.is_tmpfs {
                Some(FloorReason::Tmpfs)
            } else if facts.in_kubernetes {
                Some(FloorReason::Kubernetes)
            } else {
                None
            };
            match reason {
                Some(reason) => {
                    warnings.push(LimitWarning::FreeSpaceUnreliable { reason });
                    (SPOOL_FLOOR_BYTES, Source::Auto, SpoolBasis::Floor(reason))
                }
                None => {
                    let percent = if facts.shares_device_with_data {
                        AUTO_SHARED_DEVICE_PERCENT
                    } else {
                        AUTO_SHARE_PERCENT
                    };
                    let (b, basis) = share(percent);
                    if b > available {
                        warnings.push(LimitWarning::FloorAboveAvailable { available });
                    }
                    (b, Source::Auto, basis)
                }
            }
        }
    };
    // An explicit budget against the space that the disk has.
    if spool_source == Source::Explicit && facts.measured() {
        let limit = if facts.shares_device_with_data {
            available
        } else {
            percent_of(available, WARN_SHARE_PERCENT)
        };
        if spool_budget > limit {
            warnings.push(LimitWarning::SpoolAboveFreeSpace {
                budget: spool_budget,
                available,
                shared_device: facts.shares_device_with_data,
            });
        }
    }
    let (per_upload, per_upload_source) = match explicit.per_upload {
        Some(n) => (n, Source::Explicit),
        None => (default_per_upload(spool_budget), Source::Auto),
    };
    let (total_multipart, total_multipart_source) = match explicit.total_multipart {
        Some(n) => (n, Source::Explicit),
        None => (default_total_multipart(spool_budget), Source::Auto),
    };
    let (max_object_size, max_object_size_source) = match explicit.max_object_size {
        BytesOrAuto::Bytes(n) => (n, Source::Explicit),
        BytesOrAuto::Auto if per_upload == 0 => (default_per_upload(spool_budget), Source::Auto),
        BytesOrAuto::Auto => (per_upload, Source::Auto),
    };
    let half = spool_budget / 2;
    if per_upload_source == Source::Explicit && per_upload > half {
        warnings.push(LimitWarning::UploadAboveHalfBudget {
            which: UploadLimit::PerUpload,
            value: per_upload,
            limit: half,
            budget: spool_budget,
        });
    }
    if max_object_size_source == Source::Explicit && max_object_size > half {
        warnings.push(LimitWarning::UploadAboveHalfBudget {
            which: UploadLimit::MaxObjectSize,
            value: max_object_size,
            limit: half,
            budget: spool_budget,
        });
    }
    (
        EffectiveLimits {
            spool_budget,
            spool_source,
            spool_basis,
            per_upload,
            per_upload_source,
            total_multipart,
            total_multipart_source,
            max_object_size,
            max_object_size_source,
        },
        warnings,
    )
}

/// A size for a person: `23 GiB`, `11.5 GiB`, `768 MiB`.
pub fn human_bytes(n: u64) -> String {
    if n >= GIB {
        let g = n as f64 / GIB as f64;
        let s = format!("{g:.1}");
        format!("{} GiB", s.strip_suffix(".0").unwrap_or(&s))
    } else if n >= MIB {
        format!("{} MiB", n / MIB)
    } else {
        format!("{n} bytes")
    }
}

impl LimitWarning {
    /// The warning text. `dir` is the spool directory.
    pub fn message(&self, dir: &Path) -> String {
        let dir = dir.display();
        match *self {
            Self::FreeSpaceUnreliable { reason } => {
                let why = match reason {
                    FloorReason::Tmpfs => format!(
                        "{dir} is on a tmpfs, so its free space is memory and not a disk size"
                    ),
                    FloorReason::Kubernetes => format!(
                        "the proxy runs under Kubernetes, where the free space of {dir} does \
                         not show an emptyDir sizeLimit"
                    ),
                    FloorReason::Unmeasured => {
                        format!("the free space of {dir} could not be measured")
                    }
                };
                format!(
                    "DGP_SPOOL_MAX_BYTES is not set and {why}. The spool budget is the floor of \
                     {}. Set DGP_SPOOL_MAX_BYTES to the space that the directory really has \
                     (a number of bytes, or a percent such as 40%).",
                    human_bytes(SPOOL_FLOOR_BYTES)
                )
            }
            Self::FloorAboveAvailable { available } => format!(
                "The free space of {dir} is {}, below the spool floor of {}. Free disk space \
                 or set DGP_SPOOL_DIR to a larger disk.",
                human_bytes(available),
                human_bytes(SPOOL_FLOOR_BYTES)
            ),
            Self::SpoolAboveFreeSpace {
                budget,
                available,
                shared_device,
            } => {
                let rule = if shared_device {
                    format!(
                        "above the free space ({}): the directory shares its device with \
                         backend data",
                        human_bytes(available)
                    )
                } else {
                    format!(
                        "above {WARN_SHARE_PERCENT}% of the free space ({})",
                        human_bytes(available)
                    )
                };
                format!(
                    "DGP_SPOOL_MAX_BYTES is {} ({budget} bytes), {rule} of {dir} at boot. \
                     Uploads can fill the disk before the budget is full. Lower \
                     DGP_SPOOL_MAX_BYTES, or set it to auto.",
                    human_bytes(budget)
                )
            }
            Self::UploadAboveHalfBudget {
                which,
                value,
                limit,
                budget,
            } => {
                let (name, fix) = match which {
                    UploadLimit::PerUpload => (
                        "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES",
                        "Lower it, or raise DGP_SPOOL_MAX_BYTES",
                    ),
                    UploadLimit::MaxObjectSize => (
                        "max_object_size",
                        "Lower it (or set it to auto), or raise DGP_SPOOL_MAX_BYTES",
                    ),
                };
                format!(
                    "{name} is {} ({value} bytes), above half of the spool budget \
                     ({} of {}, spool directory {dir}). An upload of that size cannot fit. {fix}.",
                    human_bytes(value),
                    human_bytes(limit),
                    human_bytes(budget)
                )
            }
        }
    }
}

impl EffectiveLimits {
    /// How the spool budget was found, for a log line and a 413 message.
    pub fn spool_note(&self, dir: &Path) -> String {
        match (self.spool_source, self.spool_basis) {
            (_, SpoolBasis::Share { percent, available }) => {
                let kind = if self.spool_source == Source::Auto {
                    "auto"
                } else {
                    "set as a percent"
                };
                format!(
                    "{kind}: {percent}% of {} free on {} at boot",
                    human_bytes(available),
                    dir.display()
                )
            }
            (_, SpoolBasis::Floor(_)) => {
                "auto: the floor, because the free space is not a reliable size here".to_string()
            }
            (_, SpoolBasis::Bytes) => "set explicitly".to_string(),
        }
    }

    /// The boot log line.
    pub fn boot_line(&self, dir: &Path) -> String {
        let per_upload = if self.per_upload == 0 {
            "no per-upload limit".to_string()
        } else {
            format!("per-upload limit {}", human_bytes(self.per_upload))
        };
        let mos = match self.max_object_size_source {
            Source::Auto => format!(
                "max_object_size auto = {}",
                human_bytes(self.max_object_size)
            ),
            Source::Explicit => format!("max_object_size {}", human_bytes(self.max_object_size)),
        };
        format!(
            "spool budget {} = {}; {per_upload}; total multipart limit {}; {mos}",
            human_bytes(self.spool_budget),
            self.spool_note(dir),
            human_bytes(self.total_multipart),
        )
    }
}

/// The limits as one config snapshot has them, with the inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLimits {
    pub effective: EffectiveLimits,
    pub warnings: Vec<LimitWarning>,
    /// The spool directory.
    pub dir: PathBuf,
    pub facts: DiskFacts,
    /// The variables of [`LIMIT_ENV_VARS`] that were set when the limits
    /// were derived (the source of their value is `env`, not the file).
    pub env_set: Vec<&'static str>,
}

/// The variables that set a limit.
pub const LIMIT_ENV_VARS: &[&str] = &[
    "DGP_SPOOL_MAX_BYTES",
    "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES",
    "DGP_MAX_TOTAL_MULTIPART_BYTES",
    "DGP_MAX_OBJECT_SIZE",
];

impl ResolvedLimits {
    pub fn warning_messages(&self) -> Vec<String> {
        self.warnings.iter().map(|w| w.message(&self.dir)).collect()
    }

    /// The `limits` object of `GET /_/api/admin/config`.
    pub fn report(&self) -> LimitsReport {
        let e = &self.effective;
        let label = |source: Source, var: &str| match source {
            Source::Auto => "auto",
            Source::Explicit if self.env_set.contains(&var) => "env",
            Source::Explicit => "explicit",
        };
        let value = |bytes: u64, source: &'static str, detail: String| LimitValue {
            bytes,
            source,
            detail,
        };
        let set_by = |var: &str, bytes: u64| {
            if bytes == 0 {
                format!("set by {var}: no cap")
            } else {
                format!("set by {var}")
            }
        };
        LimitsReport {
            spool_dir: self.dir.display().to_string(),
            free_bytes: self.facts.free,
            total_bytes: self.facts.total,
            own_spool_bytes: self.facts.own_spool_bytes,
            on_tmpfs: self.facts.is_tmpfs,
            shares_device_with_data: self.facts.shares_device_with_data,
            in_kubernetes: self.facts.in_kubernetes,
            spool_budget: value(
                e.spool_budget,
                label(e.spool_source, "DGP_SPOOL_MAX_BYTES"),
                e.spool_note(&self.dir),
            ),
            per_upload: value(
                e.per_upload,
                label(e.per_upload_source, "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES"),
                match e.per_upload_source {
                    Source::Auto => "auto: half of the spool budget".to_string(),
                    Source::Explicit => set_by("DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES", e.per_upload),
                },
            ),
            total_multipart: value(
                e.total_multipart,
                label(e.total_multipart_source, "DGP_MAX_TOTAL_MULTIPART_BYTES"),
                match e.total_multipart_source {
                    Source::Auto => "auto: half of the spool budget".to_string(),
                    Source::Explicit => set_by("DGP_MAX_TOTAL_MULTIPART_BYTES", e.total_multipart),
                },
            ),
            max_object_size: value(
                e.max_object_size,
                label(e.max_object_size_source, "DGP_MAX_OBJECT_SIZE"),
                match e.max_object_size_source {
                    Source::Auto => "auto: the per-upload limit".to_string(),
                    Source::Explicit => "set by max_object_size".to_string(),
                },
            ),
            warnings: self.warning_messages(),
        }
    }
}

/// One effective limit and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LimitValue {
    /// 0 = no cap (per-upload limit only).
    pub bytes: u64,
    /// `auto`, `explicit` (the YAML file) or `env` (a `DGP_*` variable).
    pub source: &'static str,
    /// The inputs, in words.
    pub detail: String,
}

/// The effective limits and their inputs, as the admin API shows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LimitsReport {
    pub spool_dir: String,
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub own_spool_bytes: u64,
    pub on_tmpfs: bool,
    pub shares_device_with_data: bool,
    pub in_kubernetes: bool,
    pub spool_budget: LimitValue,
    pub per_upload: LimitValue,
    pub total_multipart: LimitValue,
    pub max_object_size: LimitValue,
    pub warnings: Vec<String>,
}

/// The I/O half: measure the disk of `dir`. `data_roots` are the filesystem
/// backend roots and the data directory. A measure that fails gives
/// `total: 0` (unmeasured). A debug build with `DGP_TEST_DISK_FREE_BYTES`
/// fakes the whole result (free space as given, no tmpfs, no Kubernetes, no
/// shared device) so a test sees one answer on every host.
pub fn measure_disk_facts(dir: &Path, data_roots: &[PathBuf], env: EnvLookup) -> DiskFacts {
    let own_spool_bytes = own_spool_bytes(dir);
    if let Some(free) = super::test_seams::test_seams().disk_free_bytes {
        return DiskFacts {
            free,
            total: free.max(1).saturating_mul(2),
            own_spool_bytes,
            is_tmpfs: false,
            shares_device_with_data: false,
            in_kubernetes: false,
        };
    }
    let in_kubernetes = env("KUBERNETES_SERVICE_HOST").is_some_and(|v| !v.is_empty());
    #[cfg(unix)]
    {
        let (free, total, is_tmpfs) = unix::statvfs_facts(dir).unwrap_or((0, 0, false));
        DiskFacts {
            free,
            total,
            own_spool_bytes,
            is_tmpfs,
            shares_device_with_data: unix::shares_device(dir, data_roots),
            in_kubernetes,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = data_roots;
        DiskFacts {
            free: 0,
            total: 0,
            own_spool_bytes,
            is_tmpfs: false,
            shares_device_with_data: false,
            in_kubernetes,
        }
    }
}

/// Bytes of the files that the spool directory holds now.
fn own_spool_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(unix)]
mod unix {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::path::{Path, PathBuf};

    /// `TMPFS_MAGIC` of `statfs(2)`.
    const TMPFS_MAGIC: i64 = 0x0102_1994;

    /// (free, total, is tmpfs) of the filesystem that holds `dir`.
    pub(super) fn statvfs_facts(dir: &Path) -> Option<(u64, u64, bool)> {
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `path` is a valid NUL-terminated path and `vfs` a writable
        // out-parameter.
        let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut vfs) } != 0 {
            return None;
        }
        // The field widths differ by platform (u32 on some).
        #[allow(clippy::useless_conversion)]
        let (frsize, avail, blocks) = (
            u64::from(vfs.f_frsize).max(1),
            u64::from(vfs.f_bavail),
            u64::from(vfs.f_blocks),
        );
        let free = avail.saturating_mul(frsize);
        let total = blocks.saturating_mul(frsize);
        Some((free, total, is_tmpfs(&path)))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn is_tmpfs(path: &std::ffi::CStr) -> bool {
        // SAFETY: as above.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut fs) } != 0 {
            return false;
        }
        #[allow(clippy::unnecessary_cast)]
        {
            fs.f_type as i64 == TMPFS_MAGIC
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn is_tmpfs(_path: &std::ffi::CStr) -> bool {
        false
    }

    /// The device of `path`, or of its nearest existing parent.
    fn device_of(path: &Path) -> Option<u64> {
        let mut p: PathBuf = path.to_path_buf();
        loop {
            if let Ok(m) = std::fs::metadata(&p) {
                return Some(m.dev());
            }
            if !p.pop() {
                return None;
            }
        }
    }

    pub(super) fn shares_device(dir: &Path, roots: &[PathBuf]) -> bool {
        let Some(dev) = device_of(dir) else {
            return false;
        };
        roots.iter().any(|r| device_of(r) == Some(dev))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn facts(free: u64) -> DiskFacts {
        DiskFacts {
            free,
            total: free.saturating_mul(2).max(1),
            own_spool_bytes: 0,
            is_tmpfs: false,
            shares_device_with_data: false,
            in_kubernetes: false,
        }
    }

    fn auto() -> ExplicitLimits {
        ExplicitLimits::default()
    }

    #[test]
    fn settings_parse() {
        assert_eq!("auto".parse(), Ok(SpoolSetting::Auto));
        assert_eq!(" AUTO ".parse(), Ok(SpoolSetting::Auto));
        assert_eq!("40%".parse(), Ok(SpoolSetting::Percent(40)));
        assert_eq!("100%".parse(), Ok(SpoolSetting::Percent(100)));
        assert_eq!("1024".parse(), Ok(SpoolSetting::Bytes(1024)));
        for bad in ["0%", "101%", "-1", "12 GiB", "", "%"] {
            assert!(bad.parse::<SpoolSetting>().is_err(), "{bad}");
        }
        assert_eq!("auto".parse(), Ok(BytesOrAuto::Auto));
        assert_eq!("7".parse(), Ok(BytesOrAuto::Bytes(7)));
        assert!("40%".parse::<BytesOrAuto>().is_err());
    }

    #[test]
    fn bytes_or_auto_round_trips_through_yaml() {
        for v in [BytesOrAuto::Auto, BytesOrAuto::Bytes(104_857_600)] {
            let y = serde_yaml::to_string(&v).unwrap();
            assert_eq!(serde_yaml::from_str::<BytesOrAuto>(&y).unwrap(), v, "{y}");
        }
        assert_eq!(
            serde_yaml::from_str::<BytesOrAuto>("auto").unwrap(),
            BytesOrAuto::Auto
        );
        assert!(serde_yaml::from_str::<BytesOrAuto>("big").is_err());
        assert!(serde_yaml::from_str::<BytesOrAuto>("-5").is_err());
    }

    /// Every rule of the derivation, one row each.
    #[test]
    fn the_rules_table() {
        // Worked example of the docs: 60 GiB free.
        let (l, w) = derive_limits(facts(60 * GIB), auto());
        assert_eq!(l.spool_budget, 30 * GIB);
        assert_eq!(l.per_upload, 15 * GIB);
        assert_eq!(l.total_multipart, 15 * GIB);
        assert_eq!(l.max_object_size, 15 * GIB);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(l.spool_source, Source::Auto);

        // Own spool files count as free.
        let mut f = facts(40 * GIB);
        f.own_spool_bytes = 20 * GIB;
        assert_eq!(derive_limits(f, auto()).0.spool_budget, 30 * GIB);

        // 25% on a device shared with backend data.
        let mut f = facts(60 * GIB);
        f.shares_device_with_data = true;
        assert_eq!(derive_limits(f, auto()).0.spool_budget, 15 * GIB);

        // Whole MiB, rounded down.
        let (l, _) = derive_limits(facts(10 * GIB + 3 * MIB + 5), auto());
        assert_eq!(l.spool_budget % MIB, 0);
        assert_eq!(l.spool_budget, 5 * GIB + MIB);

        // The floor, with a warning when the disk is smaller than it.
        let (l, w) = derive_limits(facts(GIB / 2), auto());
        assert_eq!(l.spool_budget, GIB);
        assert_eq!(
            w,
            vec![LimitWarning::FloorAboveAvailable { available: GIB / 2 }]
        );
        let (l, w) = derive_limits(facts(1500 * MIB), auto());
        assert_eq!(l.spool_budget, GIB);
        assert!(w.is_empty());

        // A tmpfs, Kubernetes, or a failed measure: floor and one warning.
        for (tmpfs, k8s, measured, reason) in [
            (true, false, true, FloorReason::Tmpfs),
            (false, true, true, FloorReason::Kubernetes),
            (false, false, false, FloorReason::Unmeasured),
        ] {
            let mut f = facts(500 * GIB);
            f.is_tmpfs = tmpfs;
            f.in_kubernetes = k8s;
            if !measured {
                f.total = 0;
            }
            let (l, w) = derive_limits(f, auto());
            assert_eq!(l.spool_budget, GIB, "{reason:?}");
            assert_eq!(w, vec![LimitWarning::FreeSpaceUnreliable { reason }]);
            assert_eq!(l.spool_basis, SpoolBasis::Floor(reason));
        }

        // A percent is that share of the free space (own files included).
        let mut ex = auto();
        ex.spool = SpoolSetting::Percent(40);
        let (l, w) = derive_limits(facts(100 * GIB), ex);
        assert_eq!(l.spool_budget, 40 * GIB);
        assert_eq!(l.spool_source, Source::Explicit);
        assert!(w.is_empty());
        // A percent also works on a tmpfs: the operator chose it.
        let mut f = facts(100 * GIB);
        f.is_tmpfs = true;
        assert_eq!(derive_limits(f, ex).0.spool_budget, 40 * GIB);

        // Explicit bytes win, even below the floor.
        ex.spool = SpoolSetting::Bytes(512 * MIB);
        let (l, _) = derive_limits(facts(100 * GIB), ex);
        assert_eq!(l.spool_budget, 512 * MIB);
        assert_eq!(l.per_upload, 256 * MIB);
        assert_eq!(l.max_object_size, 256 * MIB);

        // Explicit per-upload, total and max_object_size win.
        let ex = ExplicitLimits {
            spool: SpoolSetting::Bytes(10 * GIB),
            per_upload: Some(3 * GIB),
            total_multipart: Some(4 * GIB),
            max_object_size: BytesOrAuto::Bytes(2 * GIB),
        };
        let (l, w) = derive_limits(facts(100 * GIB), ex);
        assert_eq!(
            (l.per_upload, l.total_multipart, l.max_object_size),
            (3 * GIB, 4 * GIB, 2 * GIB)
        );
        assert_eq!(l.per_upload_source, Source::Explicit);
        assert_eq!(l.max_object_size_source, Source::Explicit);
        assert!(w.is_empty(), "{w:?}");

        // No per-upload cap: auto max_object_size falls back to half.
        let ex = ExplicitLimits {
            per_upload: Some(0),
            ..auto()
        };
        let (l, _) = derive_limits(facts(60 * GIB), ex);
        assert_eq!(l.per_upload, 0);
        assert_eq!(l.max_object_size, 15 * GIB);
    }

    #[test]
    fn warnings_table() {
        let dir = Path::new("/var/spool/dgp");
        // An explicit budget above 90% of the free space.
        let ex = |n| ExplicitLimits {
            spool: SpoolSetting::Bytes(n),
            ..auto()
        };
        let (_, w) = derive_limits(facts(100 * GIB), ex(91 * GIB));
        assert!(matches!(
            w[..],
            [LimitWarning::SpoolAboveFreeSpace {
                shared_device: false,
                ..
            }]
        ));
        let (_, w) = derive_limits(facts(100 * GIB), ex(90 * GIB));
        assert!(w.is_empty(), "90% exactly is fine: {w:?}");
        // Shared device: above the free space itself.
        let mut f = facts(100 * GIB);
        f.shares_device_with_data = true;
        assert!(derive_limits(f, ex(100 * GIB)).1.is_empty());
        assert!(matches!(
            derive_limits(f, ex(101 * GIB)).1[..],
            [LimitWarning::SpoolAboveFreeSpace {
                shared_device: true,
                ..
            }]
        ));
        // The message names the directory, the numbers and the setting.
        let m = derive_limits(facts(100 * GIB), ex(95 * GIB)).1[0].message(dir);
        for needle in ["/var/spool/dgp", "DGP_SPOOL_MAX_BYTES", "95 GiB", "100 GiB"] {
            assert!(m.contains(needle), "{needle} in {m}");
        }
        // An unmeasured disk has no free-space rule.
        let mut f = facts(100 * GIB);
        f.total = 0;
        assert!(derive_limits(f, ex(500 * GIB)).1.is_empty());

        // An upload limit above half of the budget.
        let ex = ExplicitLimits {
            spool: SpoolSetting::Bytes(10 * GIB),
            per_upload: Some(6 * GIB),
            max_object_size: BytesOrAuto::Bytes(8 * GIB),
            ..auto()
        };
        let w = derive_limits(facts(100 * GIB), ex).1;
        assert_eq!(w.len(), 2, "{w:?}");
        let text: Vec<_> = w.iter().map(|w| w.message(dir)).collect();
        assert!(text[0].contains("DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES") && text[0].contains("6 GiB"));
        assert!(text[1].contains("max_object_size") && text[1].contains("5 GiB"));
        // Exactly half is fine.
        let ex = ExplicitLimits {
            spool: SpoolSetting::Bytes(10 * GIB),
            max_object_size: BytesOrAuto::Bytes(5 * GIB),
            ..auto()
        };
        assert!(derive_limits(facts(100 * GIB), ex).1.is_empty());
    }

    /// The Helm chart and the operator set `DGP_SPOOL_MAX_BYTES` to three
    /// quarters of the `/tmp` size limit. Under Kubernetes, with the free
    /// space of `/tmp` at most that limit, that explicit value raises no
    /// warning for any limit.
    #[test]
    fn the_chart_and_operator_spool_value_raises_no_warning() {
        for tmp in [256 * MIB, 512 * MIB, GIB, 2 * GIB, 10 * GIB] {
            let mut f = facts(tmp);
            f.in_kubernetes = true;
            f.is_tmpfs = true;
            let ex = ExplicitLimits {
                spool: SpoolSetting::Bytes(tmp / 4 * 3),
                ..auto()
            };
            let (l, w) = derive_limits(f, ex);
            assert!(w.is_empty(), "{tmp}: {w:?}");
            assert_eq!(l.spool_budget, tmp / 4 * 3);
            assert_eq!(l.max_object_size, l.spool_budget / 2);
        }
    }

    /// A config apply derives again from the boot facts: a changed
    /// `max_object_size` takes effect, the spool budget does not move, and
    /// without boot facts `auto` is the default of earlier releases.
    #[test]
    fn an_apply_derives_again_and_the_budget_stays() {
        use crate::config::Config;
        let boot = crate::deltaglider::spool::BootLimits {
            dir: "/spool".into(),
            facts: facts(60 * GIB),
            budget_note: String::new(),
        };
        let env = |_: &str| None;
        let mut c = Config::default();
        assert_eq!(c.max_object_size_bytes(), UNMEASURED_MAX_OBJECT_SIZE);
        c.resolve_limits_from(None, &env);
        assert!(c.tuning.limits.is_none());

        c.resolve_limits_from(Some(boot.clone()), &env);
        assert_eq!(c.max_object_size_bytes(), 15 * GIB);
        let budget = c.tuning.limits.as_ref().unwrap().effective.spool_budget;
        assert_eq!(budget, 30 * GIB);

        // The operator sets a value, and an apply runs: it wins at once.
        c.max_object_size = BytesOrAuto::Bytes(2 * GIB);
        c.resolve_limits_from(Some(boot.clone()), &env);
        assert_eq!(c.max_object_size_bytes(), 2 * GIB);
        let l = c.tuning.limits.as_ref().unwrap();
        assert_eq!(l.effective.spool_budget, budget);
        assert_eq!(l.effective.max_object_size_source, Source::Explicit);

        // Back to auto.
        c.max_object_size = BytesOrAuto::Auto;
        c.resolve_limits_from(Some(boot), &|n: &str| {
            (n == "DGP_MAX_OBJECT_SIZE").then(|| "auto".to_string())
        });
        assert_eq!(c.max_object_size_bytes(), 15 * GIB);
        assert_eq!(
            c.tuning.limits.as_ref().unwrap().env_set,
            vec!["DGP_MAX_OBJECT_SIZE"]
        );
    }

    /// Class guard: the spool default is not a number in the code. The
    /// budget, and every limit that follows from it, comes from
    /// `derive_limits`.
    #[test]
    fn the_spool_budget_has_one_rule() {
        let spool = include_str!("../deltaglider/spool.rs");
        let code = &spool[..spool.find("#[cfg(test)]").unwrap()];
        assert!(
            code.contains("derive_limits("),
            "the spool derives its budget"
        );
        assert!(
            !code.contains("16 * 1024 * 1024 * 1024") && !code.contains("DEFAULT_SPOOL_MAX_BYTES"),
            "no fixed default of the budget remains"
        );
        let multipart = include_str!("../multipart.rs");
        let code = &multipart[..multipart.find("#[cfg(test)]").unwrap_or(multipart.len())];
        assert!(
            code.contains("default_per_upload(") && code.contains("default_total_multipart("),
            "the multipart defaults come from config::limits"
        );
    }

    #[test]
    fn the_boot_line_names_the_inputs() {
        let (l, _) = derive_limits(facts(46 * GIB), auto());
        let line = l.boot_line(Path::new("/tmp/dgp-spool"));
        for needle in [
            "spool budget 23 GiB",
            "50% of 46 GiB free on /tmp/dgp-spool",
            "per-upload limit 11.5 GiB",
            "max_object_size auto = 11.5 GiB",
        ] {
            assert!(line.contains(needle), "{needle} in {line}");
        }
    }

    prop_compose! {
        fn arb_facts()(
            free in prop_oneof![0u64..(4 * GIB), 0u64..(1u64 << 52)],
            own in 0u64..(1u64 << 40),
            measured in proptest::bool::weighted(0.9),
            tmpfs in any::<bool>(),
            shared in any::<bool>(),
            k8s in any::<bool>(),
        ) -> DiskFacts {
            DiskFacts {
                free,
                total: if measured { free.saturating_add(own).saturating_add(1) } else { 0 },
                own_spool_bytes: own,
                is_tmpfs: tmpfs,
                shares_device_with_data: shared,
                in_kubernetes: k8s,
            }
        }
    }

    fn arb_explicit() -> impl Strategy<Value = ExplicitLimits> {
        (
            prop_oneof![
                Just(SpoolSetting::Auto),
                (1u8..=100).prop_map(SpoolSetting::Percent),
                (0u64..(1u64 << 45)).prop_map(SpoolSetting::Bytes),
            ],
            proptest::option::of(0u64..(1u64 << 45)),
            proptest::option::of(0u64..(1u64 << 45)),
            prop_oneof![
                Just(BytesOrAuto::Auto),
                (0u64..(1u64 << 45)).prop_map(BytesOrAuto::Bytes)
            ],
        )
            .prop_map(|(spool, per_upload, total_multipart, max_object_size)| {
                ExplicitLimits {
                    spool,
                    per_upload,
                    total_multipart,
                    max_object_size,
                }
            })
    }

    proptest! {
        /// Explicit values always win.
        #[test]
        fn explicit_always_wins(f in arb_facts(), ex in arb_explicit()) {
            let (l, _) = derive_limits(f, ex);
            if let SpoolSetting::Bytes(n) = ex.spool { prop_assert_eq!(l.spool_budget, n); }
            if let Some(n) = ex.per_upload { prop_assert_eq!(l.per_upload, n); }
            if let Some(n) = ex.total_multipart { prop_assert_eq!(l.total_multipart, n); }
            if let BytesOrAuto::Bytes(n) = ex.max_object_size { prop_assert_eq!(l.max_object_size, n); }
        }

        /// With nothing set: the floor holds, the budget is whole MiB and
        /// never above what the disk has (except the floor), and a limit
        /// derived from it is at most half of it.
        #[test]
        fn auto_invariants(f in arb_facts()) {
            let (l, _) = derive_limits(f, auto());
            prop_assert!(l.spool_budget >= SPOOL_FLOOR_BYTES);
            prop_assert_eq!(l.spool_budget % MIB, 0);
            prop_assert!(l.spool_budget <= f.available().max(SPOOL_FLOOR_BYTES));
            prop_assert!(l.per_upload <= l.spool_budget / 2);
            prop_assert!(l.total_multipart <= l.spool_budget / 2);
            prop_assert!(l.max_object_size <= l.spool_budget / 2);
            prop_assert_eq!(l.max_object_size, l.per_upload);
            // The shared-device share is never above the plain share.
            let mut g = f;
            g.shares_device_with_data = false;
            let plain = derive_limits(g, auto()).0.spool_budget;
            prop_assert!(l.spool_budget <= plain || !f.shares_device_with_data);
        }

        /// A percent never gives more than the available space (or the floor).
        #[test]
        fn percent_invariants(f in arb_facts(), p in 1u8..=100) {
            let ex = ExplicitLimits { spool: SpoolSetting::Percent(p), ..auto() };
            let (l, _) = derive_limits(f, ex);
            prop_assert!(l.spool_budget >= SPOOL_FLOOR_BYTES);
            prop_assert_eq!(l.spool_budget % MIB, 0);
            prop_assert!(l.spool_budget <= f.available().max(SPOOL_FLOOR_BYTES));
        }

        /// A warning is never missing for an explicit per-upload or object
        /// limit above half of the budget, and never present for one at or
        /// below it.
        #[test]
        fn the_half_budget_warning_is_exact(f in arb_facts(), ex in arb_explicit()) {
            let (l, w) = derive_limits(f, ex);
            let has = |which| w.iter().any(|x| matches!(x, LimitWarning::UploadAboveHalfBudget { which: k, .. } if *k == which));
            prop_assert_eq!(has(UploadLimit::PerUpload),
                ex.per_upload.is_some() && l.per_upload > l.spool_budget / 2);
            prop_assert_eq!(has(UploadLimit::MaxObjectSize),
                matches!(ex.max_object_size, BytesOrAuto::Bytes(_)) && l.max_object_size > l.spool_budget / 2);
        }
    }
}
