// SPDX-License-Identifier: BUSL-1.1

//! THE storage-capacity decision. `capacity_status` turns the disk reports
//! and the spool refusal into one state; the admin endpoint, the card and the
//! banner read only its output.
//!
//! Two kinds of pause, never merged:
//! * `LargePaused` holds exactly while the spool refuses new writes. A body at
//!   or under the spool threshold still uploads, so the state carries the
//!   threshold as `paused_above`.
//! * `BackendPaused` holds while a filesystem backend disk is critical.

use serde::Serialize;

use crate::disk_watch::{Level, RoleReport, SPOOL_BUDGET_ROLE};

/// What `capacity_status` reads. The caller fills `spool_refused` from
/// `SpoolDir::write_refusal()`, the same function that refuses a real write.
/// That call may drop cached files as a side effect; the status accepts it.
#[derive(Debug, Clone, Copy)]
pub struct CapacityInputs<'a> {
    /// The last report of every role. Empty until the first tick.
    pub reports: &'a [RoleReport],
    pub spool_refused: bool,
    /// Bodies at or under this size upload with no spool space.
    pub spool_threshold: u64,
    /// The effective `max_object_size`.
    pub largest_file: u64,
}

/// A disk with its free space. Admin view only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskSpot {
    pub path: String,
    pub free_bytes: u64,
}

/// A full filesystem backend. Admin view only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PausedBackend {
    pub name: String,
    pub path: String,
    pub free_bytes: u64,
}

/// The admin view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CapacityStatus {
    /// No disk report yet.
    Checking {
        largest_file: u64,
    },
    Healthy {
        largest_file: u64,
    },
    FillingUp {
        largest_file: u64,
        disk: DiskSpot,
    },
    LargePaused {
        largest_file: u64,
        paused_above: u64,
        disk: Option<DiskSpot>,
    },
    BackendPaused {
        largest_file: u64,
        backends: Vec<PausedBackend>,
    },
}

/// The view of a session that is not an admin: no path, no backend name, no
/// free-space figure. `paused_above` is `null` unless uploads are paused by
/// the spool; for `BackendPaused` it is `null` too, because the view cannot
/// say which bucket is affected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserView {
    pub largest_file: u64,
    pub uploads_paused: bool,
    pub paused_above: Option<u64>,
}

impl CapacityStatus {
    pub fn largest_file(&self) -> u64 {
        match self {
            Self::Checking { largest_file }
            | Self::Healthy { largest_file }
            | Self::FillingUp { largest_file, .. }
            | Self::LargePaused { largest_file, .. }
            | Self::BackendPaused { largest_file, .. } => *largest_file,
        }
    }

    /// The view for a non-admin session.
    pub fn browser_view(&self) -> BrowserView {
        let (uploads_paused, paused_above) = match self {
            Self::LargePaused { paused_above, .. } => (true, Some(*paused_above)),
            Self::BackendPaused { .. } => (true, None),
            _ => (false, None),
        };
        BrowserView {
            largest_file: self.largest_file(),
            uploads_paused,
            paused_above,
        }
    }
}

fn is_spool_role(r: &RoleReport) -> bool {
    r.label == "spool" || r.label == SPOOL_BUDGET_ROLE
}

fn spot(r: &RoleReport) -> DiskSpot {
    DiskSpot {
        path: r.path.display().to_string(),
        free_bytes: r.facts.free,
    }
}

/// Pure. Precedence: `BackendPaused`, `LargePaused`, `FillingUp`, `Healthy`.
pub fn capacity_status(i: CapacityInputs<'_>) -> CapacityStatus {
    let largest_file = i.largest_file;
    let backends: Vec<PausedBackend> = i
        .reports
        .iter()
        .filter(|r| r.level == Level::Critical)
        .filter_map(|r| {
            r.label.strip_prefix("backend:").map(|name| PausedBackend {
                name: name.to_string(),
                path: r.path.display().to_string(),
                free_bytes: r.facts.free,
            })
        })
        .collect();
    if !backends.is_empty() {
        return CapacityStatus::BackendPaused {
            largest_file,
            backends,
        };
    }
    if i.spool_refused {
        // The refusing role: the worst spool report.
        let disk = i
            .reports
            .iter()
            .filter(|r| is_spool_role(r))
            .max_by_key(|r| r.level)
            .map(spot);
        return CapacityStatus::LargePaused {
            largest_file,
            paused_above: i.spool_threshold,
            disk,
        };
    }
    if i.reports.is_empty() {
        return CapacityStatus::Checking { largest_file };
    }
    // One level below the pause: spool Low, a backend High.
    let tightest = i
        .reports
        .iter()
        .filter(|r| {
            if is_spool_role(r) {
                r.level >= Level::Low
            } else {
                r.label.starts_with("backend:") && r.level >= Level::High
            }
        })
        .min_by_key(|r| r.facts.free);
    match tightest {
        Some(r) => CapacityStatus::FillingUp {
            largest_file,
            disk: spot(r),
        },
        None => CapacityStatus::Healthy { largest_file },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_watch::Facts;
    use proptest::prelude::*;
    use std::path::PathBuf;

    const LEVELS: [Level; 4] = [Level::Ok, Level::Low, Level::High, Level::Critical];

    fn report(label: &str, level: Level, free: u64) -> RoleReport {
        RoleReport {
            label: label.to_string(),
            path: PathBuf::from(format!("/disk/{}", label.replace(':', "-"))),
            facts: Facts { free, total: 1000 },
            level,
            device: 0,
        }
    }

    fn run(reports: &[RoleReport], refused: bool) -> CapacityStatus {
        capacity_status(CapacityInputs {
            reports,
            spool_refused: refused,
            spool_threshold: 16,
            largest_file: 99,
        })
    }

    fn kind(s: &CapacityStatus) -> &'static str {
        match s {
            CapacityStatus::Checking { .. } => "checking",
            CapacityStatus::Healthy { .. } => "healthy",
            CapacityStatus::FillingUp { .. } => "filling_up",
            CapacityStatus::LargePaused { .. } => "large_paused",
            CapacityStatus::BackendPaused { .. } => "backend_paused",
        }
    }

    #[test]
    fn no_report_is_checking_and_a_refusal_still_wins() {
        assert_eq!(kind(&run(&[], false)), "checking");
        assert_eq!(kind(&run(&[], true)), "large_paused");
    }

    /// Every (spool level, backend level, refusal) row. The refusal follows
    /// the real rule: the spool refuses at High and Critical.
    #[test]
    fn the_decision_table() {
        for spool in LEVELS {
            for backend in LEVELS {
                let refused = spool >= Level::High;
                let reports = [
                    report("spool", spool, 10),
                    report("backend:fs", backend, 20),
                    report("data", Level::Critical, 1),
                ];
                let want = if backend == Level::Critical {
                    "backend_paused"
                } else if refused {
                    "large_paused"
                } else if spool == Level::Low || backend == Level::High {
                    "filling_up"
                } else {
                    "healthy"
                };
                let got = run(&reports, refused);
                assert_eq!(kind(&got), want, "spool {spool:?} backend {backend:?}");
                assert_eq!(got.largest_file(), 99);
            }
        }
    }

    #[test]
    fn the_budget_role_counts_like_the_spool_disk() {
        let r = [report(SPOOL_BUDGET_ROLE, Level::Low, 5)];
        assert_eq!(kind(&run(&r, false)), "filling_up");
        let r = [
            report("spool", Level::Ok, 500),
            report(SPOOL_BUDGET_ROLE, Level::High, 5),
        ];
        let s = run(&r, true);
        let CapacityStatus::LargePaused {
            disk, paused_above, ..
        } = s
        else {
            panic!("{s:?}")
        };
        assert_eq!(paused_above, 16);
        assert_eq!(disk.unwrap().free_bytes, 5);
    }

    #[test]
    fn every_critical_backend_is_listed_and_filling_up_names_the_tightest() {
        let r = [
            report("backend:a", Level::Critical, 3),
            report("backend:b", Level::Critical, 4),
            report("backend:c", Level::Ok, 400),
        ];
        let CapacityStatus::BackendPaused { backends, .. } = run(&r, false) else {
            panic!()
        };
        let names: Vec<_> = backends.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        let r = [
            report("spool", Level::Low, 50),
            report("backend:x", Level::High, 7),
            report("backend:y", Level::Low, 1),
        ];
        let CapacityStatus::FillingUp { disk, .. } = run(&r, false) else {
            panic!()
        };
        assert_eq!(disk.free_bytes, 7);
        assert!(disk.path.ends_with("backend-x"));
    }

    fn arb_level() -> impl Strategy<Value = Level> {
        (0u8..4).prop_map(Level::from_u8)
    }

    proptest! {
        /// The paused states agree with the refusal inputs, and nothing
        /// else produces them.
        #[test]
        fn paused_states_agree_with_the_refusal_inputs(
            spool in arb_level(),
            backends in proptest::collection::vec(arb_level(), 0..4),
            refused in any::<bool>(),
            with_data in any::<bool>(),
        ) {
            let mut reports = vec![report("spool", spool, 10)];
            for (n, l) in backends.iter().enumerate() {
                reports.push(report(&format!("backend:b{n}"), *l, 20));
            }
            if with_data {
                reports.push(report("data", Level::Critical, 1));
            }
            let any_critical = backends.contains(&Level::Critical);
            let s = run(&reports, refused);
            prop_assert_eq!(matches!(s, CapacityStatus::BackendPaused { .. }), any_critical);
            prop_assert_eq!(
                matches!(s, CapacityStatus::LargePaused { .. }),
                refused && !any_critical
            );
            if let CapacityStatus::BackendPaused { backends: b, .. } = &s {
                let n = backends.iter().filter(|l| **l == Level::Critical).count();
                prop_assert_eq!(b.len(), n);
            }
            let v = s.browser_view();
            prop_assert_eq!(v.uploads_paused, any_critical || refused);
        }
    }

    #[test]
    fn the_browser_view_has_three_keys_and_no_path() {
        let spot = DiskSpot {
            path: "/secret/spool".into(),
            free_bytes: 123_456_789,
        };
        let all = [
            CapacityStatus::Checking { largest_file: 99 },
            CapacityStatus::Healthy { largest_file: 99 },
            CapacityStatus::FillingUp {
                largest_file: 99,
                disk: spot.clone(),
            },
            CapacityStatus::LargePaused {
                largest_file: 99,
                paused_above: 16,
                disk: Some(spot.clone()),
            },
            CapacityStatus::BackendPaused {
                largest_file: 99,
                backends: vec![PausedBackend {
                    name: "hetzner".into(),
                    path: "/secret/data".into(),
                    free_bytes: 123_456_789,
                }],
            },
        ];
        let mut paused = Vec::new();
        for s in &all {
            let text = serde_json::to_string(&s.browser_view()).unwrap();
            assert!(!text.contains('/') && !text.contains("hetzner"), "{text}");
            assert!(!text.contains("123456789"), "{text}");
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                ["largest_file", "paused_above", "uploads_paused"],
                "{text}"
            );
            paused.push((
                kind(s),
                v["uploads_paused"].as_bool().unwrap(),
                v["paused_above"].clone(),
            ));
        }
        assert_eq!(
            paused,
            [
                ("checking", false, serde_json::Value::Null),
                ("healthy", false, serde_json::Value::Null),
                ("filling_up", false, serde_json::Value::Null),
                ("large_paused", true, serde_json::json!(16)),
                ("backend_paused", true, serde_json::Value::Null),
            ]
        );
    }
}
