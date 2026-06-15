// SPDX-License-Identifier: GPL-3.0-only

//! Pure lifecycle planning functions.

use crate::config_sections::{LifecycleAction, LifecycleRule};
use crate::replication::{normalize_prefix, rewrite_key};
use crate::types::FileMetadata;
use chrono::{DateTime, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Every bucket a rule run WRITES to: the scanned bucket (deletes happen
/// there) and, for transition rules, the destination. The maintenance
/// write gate must defer a run when ANY of these is busy — a transition
/// PUT landing on a bucket mid-reencrypt/migrate is exactly the racing
/// write the gate exists to stop.
pub fn rule_write_buckets(rule: &LifecycleRule) -> Vec<&str> {
    let mut buckets = vec![rule.bucket.as_str()];
    if let LifecycleAction::Transition(t) = &rule.action {
        if !t.destination.bucket.trim().is_empty() && t.destination.bucket != rule.bucket {
            buckets.push(t.destination.bucket.as_str());
        }
    }
    buckets
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Apply { action: PlannedLifecycleAction },
    Skip { reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannedLifecycleAction {
    Delete,
    Transition {
        destination_bucket: String,
        destination_key: String,
        delete_source_after_success: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    NotExpired,
    Excluded,
    DgInternal,
    DirectoryMarker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    InvalidGlob {
        pattern: String,
        reason: String,
    },
    DestinationRewrite {
        key: String,
        reason: String,
    },
    UnsafeSelfMove {
        bucket: String,
        key: String,
    },
    /// The per-object age planner was called for a `retain-newest` rule, which
    /// must go through the worker's set-relative path instead. A routing bug.
    WrongActionPath {
        rule: String,
    },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::InvalidGlob { pattern, reason } => {
                write!(f, "invalid glob {:?}: {}", pattern, reason)
            }
            PlanError::DestinationRewrite { key, reason } => {
                write!(
                    f,
                    "could not rewrite lifecycle destination for {key:?}: {reason}"
                )
            }
            PlanError::UnsafeSelfMove { bucket, key } => write!(
                f,
                "unsafe lifecycle transition would copy and delete the same object {bucket}/{key}"
            ),
            PlanError::WrongActionPath { rule } => write!(
                f,
                "retain-newest rule {rule:?} reached the per-object age planner (routing bug)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

pub fn compile_rule_globs(rule: &LifecycleRule) -> Result<(GlobSet, GlobSet), PlanError> {
    Ok((
        build_globset(&rule.include_globs)?,
        build_globset(&rule.exclude_globs)?,
    ))
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, PlanError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(err) => {
                return Err(PlanError::InvalidGlob {
                    pattern: pattern.clone(),
                    reason: err.to_string(),
                });
            }
        }
    }
    builder.build().map_err(|err| PlanError::InvalidGlob {
        pattern: "<set>".to_string(),
        reason: err.to_string(),
    })
}

pub fn lifecycle_prefix(rule: &LifecycleRule) -> String {
    normalize_prefix(&rule.prefix)
}

pub fn lifecycle_action_for(
    rule: &LifecycleRule,
    key: &str,
) -> Result<PlannedLifecycleAction, PlanError> {
    match &rule.action {
        LifecycleAction::Delete => Ok(PlannedLifecycleAction::Delete),
        // retain-newest is set-relative (it ranks the whole prefix) and MUST be
        // handled by the worker's dedicated collect→rank→act path, never the
        // per-object age planner. Reaching here is a routing bug — fail loudly
        // rather than fall through to a delete.
        LifecycleAction::RetainNewest(_) => Err(PlanError::WrongActionPath {
            rule: rule.name.clone(),
        }),
        LifecycleAction::Transition(action) => {
            let destination_bucket = action.destination.bucket.trim().to_string();
            let destination_key = rewrite_key(&rule.prefix, &action.destination.prefix, key)
                .map_err(|err| PlanError::DestinationRewrite {
                    key: key.to_string(),
                    reason: err.to_string(),
                })?;
            if action.delete_source_after_success
                && destination_bucket == rule.bucket
                && destination_key == key
            {
                return Err(PlanError::UnsafeSelfMove {
                    bucket: rule.bucket.clone(),
                    key: key.to_string(),
                });
            }
            Ok(PlannedLifecycleAction::Transition {
                destination_bucket,
                destination_key,
                delete_source_after_success: action.delete_source_after_success,
            })
        }
    }
}

/// Decide whether a single engine-visible object should expire.
pub fn plan_object(
    rule: &LifecycleRule,
    key: &str,
    meta: &FileMetadata,
    expire_before: DateTime<Utc>,
    include_globs: &GlobSet,
    exclude_globs: &GlobSet,
) -> Result<Decision, PlanError> {
    if key.ends_with('/') {
        return Ok(Decision::Skip {
            reason: SkipReason::DirectoryMarker,
        });
    }

    if is_internal_key(key) {
        return Ok(Decision::Skip {
            reason: SkipReason::DgInternal,
        });
    }

    if exclude_globs.is_match(key) {
        return Ok(Decision::Skip {
            reason: SkipReason::Excluded,
        });
    }
    if !include_globs.is_empty() && !include_globs.is_match(key) {
        return Ok(Decision::Skip {
            reason: SkipReason::Excluded,
        });
    }

    if meta.created_at <= expire_before {
        Ok(Decision::Apply {
            action: lifecycle_action_for(rule, key)?,
        })
    } else {
        Ok(Decision::Skip {
            reason: SkipReason::NotExpired,
        })
    }
}

/// Defense-in-depth for keys that should never be lifecycle targets.
///
/// Engine listings normally expose user objects, not storage artifacts. This
/// still protects config-sync data and any legacy/raw artifact that might leak
/// through a backend-specific listing path.
pub fn is_internal_key(key: &str) -> bool {
    key == ".deltaglider"
        || key.starts_with(".deltaglider/")
        || key.contains("/.deltaglider/")
        || key == ".dg"
        || key.starts_with(".dg/")
        || key.contains("/.dg/")
        || key.ends_with("/reference.bin")
        || key == "reference.bin"
        || key.ends_with(".delta")
}

// ───────────────────────── retain-newest (count-based) ─────────────────────────
//
// "Keep the newest N qualifying objects in a prefix, delete the rest." This is a
// SET-RELATIVE decision — unlike age expiry, an object's fate depends on its rank
// within the whole candidate set — so it cannot use the per-object `plan_object`
// path. The worker collects the full candidate set first, then calls the pure
// function below. ALL of the data-loss-sensitive logic lives here so it is
// exhaustively unit-tested without a server.

/// One object in a prefix, reduced to what the retain decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: String,
    pub created_at: DateTime<Utc>,
    /// Original (hydrated) object size in bytes — NOT the delta-stored size.
    pub size: u64,
}

/// Eligibility filter. An object must pass EVERY set field to be considered.
/// A `None` field is "no filter on this dimension".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QualifySpec {
    /// Minimum original size in bytes (inclusive). `Some(0)` is a no-op filter.
    pub min_size_bytes: Option<u64>,
    /// Object must be strictly older than this (i.e. `created_at <= now - min_age`).
    pub min_age: Option<chrono::Duration>,
}

impl QualifySpec {
    /// Does this candidate qualify to be counted/ranked at `now`?
    fn admits(&self, c: &Candidate, now: DateTime<Utc>) -> Option<IneligibleReason> {
        if let Some(min) = self.min_size_bytes {
            if c.size < min {
                return Some(IneligibleReason::BelowMinSize);
            }
        }
        if let Some(min_age) = self.min_age {
            // Eligible only once it is at least `min_age` old.
            if c.created_at > now - min_age {
                return Some(IneligibleReason::BelowMinAge);
            }
        }
        None
    }
}

/// Why a candidate was excluded from the count (surfaced in preview).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IneligibleReason {
    BelowMinSize,
    BelowMinAge,
}

/// The full disposition of a prefix under a retain-newest rule. Every input
/// candidate lands in EXACTLY ONE bucket — the sum of the four is the input.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetainPlan {
    /// The newest `count` qualifying objects. Never deleted.
    pub keep: Vec<Candidate>,
    /// Failed `qualify` — neither counted nor deleted (e.g. the empty README).
    pub ignored: Vec<(Candidate, IneligibleReason)>,
    /// Qualifying, outside the newest `count`, and not spared by the guard.
    pub delete: Vec<Candidate>,
    /// Qualifying and slated for deletion, but younger than
    /// `protect_younger_than` — spared THIS run only.
    pub protected: Vec<Candidate>,
}

/// Plan a `retain-newest` rule over the complete candidate set.
///
/// Steps, in order (the ordering is the safety property):
/// 1. Partition off candidates failing `qualify` into `ignored`. They can NEVER
///    be kept and NEVER be deleted — so an accidental empty/truncated file can't
///    anchor the keep set or displace a real backup.
/// 2. Rank the qualifying set by `(created_at desc, key desc)` — deterministic,
///    stable across runs (no "random survivor" on equal timestamps).
/// 3. `keep` = the first `count` of the ranked set.
/// 4. Of the remainder, any younger than `protect_younger_than` go to
///    `protected` (spared this run); the rest go to `delete`.
///
/// `count == 0` would put every qualifying object in `delete`. Callers MUST NOT
/// invoke this with `count == 0`: the config `Deserialize` impl hard-rejects it,
/// and the worker guards against it before calling here. This function stays a
/// pure mapping and does not itself special-case 0 (so a test can still exercise
/// the raw partition behaviour) — the safety lives at the two gates above.
pub fn plan_retain_newest(
    candidates: &[Candidate],
    count: u32,
    qualify: &QualifySpec,
    protect_younger_than: Option<chrono::Duration>,
    now: DateTime<Utc>,
) -> RetainPlan {
    let mut plan = RetainPlan::default();

    // 1. Eligibility partition.
    let mut eligible: Vec<&Candidate> = Vec::with_capacity(candidates.len());
    for c in candidates {
        match qualify.admits(c, now) {
            Some(reason) => plan.ignored.push((c.clone(), reason)),
            None => eligible.push(c),
        }
    }

    // 2. Rank newest-first, key-desc tie-break (total order → stable & reproducible).
    eligible.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.key.cmp(&a.key))
    });

    // 3 + 4. Keep the first `count`; guard or delete the rest.
    let keep_n = count as usize;
    let protect_cutoff = protect_younger_than.map(|d| now - d);
    for (idx, c) in eligible.into_iter().enumerate() {
        if idx < keep_n {
            plan.keep.push(c.clone());
        } else if protect_cutoff.is_some_and(|cutoff| c.created_at > cutoff) {
            plan.protected.push(c.clone());
        } else {
            plan.delete.push(c.clone());
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn meta_at(ts: i64) -> FileMetadata {
        let mut meta = FileMetadata::new_passthrough(
            "x".to_string(),
            "sha".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
            1,
            None,
        );
        meta.created_at = Utc.timestamp_opt(ts, 0).unwrap();
        meta
    }

    fn rule(include: &[&str], exclude: &[&str]) -> LifecycleRule {
        LifecycleRule {
            name: "expire-old".to_string(),
            enabled: true,
            bucket: "b".to_string(),
            prefix: String::new(),
            action: Default::default(),
            expire_after: Some("30d".to_string()),
            include_globs: include.iter().map(|s| s.to_string()).collect(),
            exclude_globs: exclude.iter().map(|s| s.to_string()).collect(),
            batch_size: 100,
        }
    }

    fn globs(include: &[&str], exclude: &[&str]) -> (LifecycleRule, GlobSet, GlobSet) {
        let rule = rule(include, exclude);
        let sets = compile_rule_globs(&rule).unwrap();
        (rule, sets.0, sets.1)
    }

    fn assert_delete(decision: Result<Decision, PlanError>) {
        assert_eq!(
            decision.unwrap(),
            Decision::Apply {
                action: PlannedLifecycleAction::Delete
            }
        );
    }

    fn assert_skip(decision: Result<Decision, PlanError>, reason: SkipReason) {
        assert_eq!(decision.unwrap(), Decision::Skip { reason });
    }

    #[test]
    fn expires_objects_older_than_cutoff() {
        let (rule, inc, exc) = globs(&[], &[]);
        let cutoff = Utc.timestamp_opt(1_000, 0).unwrap();
        assert_delete(plan_object(
            &rule,
            "old.txt",
            &meta_at(999),
            cutoff,
            &inc,
            &exc,
        ));
        assert_skip(
            plan_object(&rule, "new.txt", &meta_at(1_001), cutoff, &inc, &exc),
            SkipReason::NotExpired,
        );
    }

    #[test]
    fn honors_include_and_exclude_globs() {
        let (rule, inc, exc) = globs(&["logs/**"], &["logs/keep/**"]);
        let cutoff = Utc.timestamp_opt(1_000, 0).unwrap();
        assert_delete(plan_object(
            &rule,
            "logs/a.txt",
            &meta_at(1),
            cutoff,
            &inc,
            &exc,
        ));
        assert_skip(
            plan_object(&rule, "tmp/a.txt", &meta_at(1), cutoff, &inc, &exc),
            SkipReason::Excluded,
        );
        assert_skip(
            plan_object(&rule, "logs/keep/a.txt", &meta_at(1), cutoff, &inc, &exc),
            SkipReason::Excluded,
        );
    }

    #[test]
    fn skips_internal_keys_and_directory_markers() {
        let (rule, inc, exc) = globs(&[], &[]);
        let cutoff = Utc.timestamp_opt(1_000, 0).unwrap();
        for key in [
            ".deltaglider/config.db",
            "nested/.deltaglider/config.db",
            ".dg/reference.bin",
            "prefix/.dg/file.delta",
            "reference.bin",
            "prefix/reference.bin",
            "object.delta",
        ] {
            assert_skip(
                plan_object(&rule, key, &meta_at(1), cutoff, &inc, &exc),
                SkipReason::DgInternal,
            );
        }
        assert_skip(
            plan_object(&rule, "folder/", &meta_at(1), cutoff, &inc, &exc),
            SkipReason::DirectoryMarker,
        );
    }

    #[test]
    fn lifecycle_prefix_normalizes_slashes() {
        let (rule, inc, exc) = globs(&[], &[]);
        let cutoff = Utc::now() - Duration::days(30);
        assert!(matches!(
            plan_object(
                &rule,
                "a",
                &meta_at(cutoff.timestamp() - 1),
                cutoff,
                &inc,
                &exc
            ),
            Ok(Decision::Apply {
                action: PlannedLifecycleAction::Delete
            })
        ));

        let cfg_rule = LifecycleRule {
            name: "r".into(),
            enabled: true,
            bucket: "b".into(),
            prefix: "/a//b".into(),
            action: Default::default(),
            expire_after: Some("1d".into()),
            include_globs: vec![],
            exclude_globs: vec![],
            batch_size: 100,
        };
        assert_eq!(lifecycle_prefix(&cfg_rule), "a/b/");
    }

    #[test]
    fn transition_action_rewrites_destination_and_normalizes_prefixes() {
        let mut rule = rule(&[], &[]);
        rule.bucket = "src".into();
        rule.prefix = "/live//builds".into();
        rule.action =
            LifecycleAction::Transition(crate::config_sections::LifecycleTransitionAction {
                destination: crate::config_sections::LifecycleDestination {
                    bucket: "archive".into(),
                    prefix: "/cold//2026".into(),
                },
                delete_source_after_success: false,
            });

        assert_eq!(
            lifecycle_action_for(&rule, "live/builds/app.zip").unwrap(),
            PlannedLifecycleAction::Transition {
                destination_bucket: "archive".into(),
                destination_key: "cold/2026/app.zip".into(),
                delete_source_after_success: false,
            }
        );
    }

    #[test]
    fn transition_action_blocks_delete_source_self_move() {
        let mut rule = rule(&[], &[]);
        rule.bucket = "b".into();
        rule.prefix = "same".into();
        rule.action =
            LifecycleAction::Transition(crate::config_sections::LifecycleTransitionAction {
                destination: crate::config_sections::LifecycleDestination {
                    bucket: "b".into(),
                    prefix: "same".into(),
                },
                delete_source_after_success: true,
            });

        assert!(matches!(
            lifecycle_action_for(&rule, "same/file.txt"),
            Err(PlanError::UnsafeSelfMove { .. })
        ));
    }

    // ───────────────────── retain-newest (plan_retain_newest) ─────────────────────
    //
    // This is the function that DELETES DATA, so the suite is adversarial: it
    // proves junk can never anchor the keep set, that an all-junk prefix is a safe
    // no-op, and that keep/ignore/delete/protected partition the input exactly.

    fn cand(key: &str, ts: i64, size: u64) -> Candidate {
        Candidate {
            key: key.to_string(),
            created_at: Utc.timestamp_opt(ts, 0).unwrap(),
            size,
        }
    }

    fn keys(cs: &[Candidate]) -> Vec<&str> {
        cs.iter().map(|c| c.key.as_str()).collect()
    }

    const NOW: i64 = 1_000_000;
    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(NOW, 0).unwrap()
    }

    /// Every input candidate must land in exactly one output bucket.
    fn assert_partitions(input: &[Candidate], plan: &RetainPlan) {
        let total = plan.keep.len() + plan.ignored.len() + plan.delete.len() + plan.protected.len();
        assert_eq!(
            total,
            input.len(),
            "partition lost or duplicated candidates: {plan:?}"
        );
    }

    #[test]
    fn retain_newest_keeps_k_newest_deletes_rest() {
        // 5 dated dumps, keep 2. Newest two kept, oldest three deleted.
        let input = vec![
            cand("d1", 100, 6_000_000),
            cand("d2", 200, 6_000_000),
            cand("d3", 300, 6_000_000),
            cand("d4", 400, 6_000_000),
            cand("d5", 500, 6_000_000),
        ];
        let plan = plan_retain_newest(&input, 2, &QualifySpec::default(), None, now());
        assert_partitions(&input, &plan);
        assert_eq!(keys(&plan.keep), vec!["d5", "d4"]);
        assert_eq!(keys(&plan.delete), vec!["d3", "d2", "d1"]);
        assert!(plan.ignored.is_empty() && plan.protected.is_empty());
    }

    #[test]
    fn retain_newest_under_count_is_noop() {
        let input = vec![cand("d1", 100, 10), cand("d2", 200, 10)];
        let plan = plan_retain_newest(&input, 5, &QualifySpec::default(), None, now());
        assert_partitions(&input, &plan);
        assert!(plan.delete.is_empty(), "must not delete when count >= N");
        assert_eq!(plan.keep.len(), 2);
    }

    #[test]
    fn retain_newest_count_one_keeps_single_newest() {
        let input = vec![cand("a", 100, 10), cand("b", 300, 10), cand("c", 200, 10)];
        let plan = plan_retain_newest(&input, 1, &QualifySpec::default(), None, now());
        assert_eq!(keys(&plan.keep), vec!["b"]); // ts 300 is newest
        assert_eq!(keys(&plan.delete), vec!["c", "a"]);
    }

    #[test]
    fn retain_newest_empty_input_is_noop() {
        let plan = plan_retain_newest(&[], 2, &QualifySpec::default(), None, now());
        assert_eq!(plan, RetainPlan::default());
    }

    #[test]
    fn retain_newest_equal_timestamps_break_ties_by_key_desc_stably() {
        // All same timestamp → deterministic key-desc order, count 2.
        let input = vec![
            cand("alpha", 100, 10),
            cand("bravo", 100, 10),
            cand("charlie", 100, 10),
        ];
        let plan = plan_retain_newest(&input, 2, &QualifySpec::default(), None, now());
        // key desc: charlie, bravo, alpha → keep {charlie, bravo}, delete {alpha}
        assert_eq!(keys(&plan.keep), vec!["charlie", "bravo"]);
        assert_eq!(keys(&plan.delete), vec!["alpha"]);
        // Re-running yields the same survivor — never random.
        let plan2 = plan_retain_newest(&input, 2, &QualifySpec::default(), None, now());
        assert_eq!(plan, plan2);
    }

    #[test]
    fn retain_newest_min_size_ignores_junk_and_protects_real_backups() {
        // THE HEADLINE CASE. dump-A newest, then a 0-byte README, then two more
        // real dumps. With min_size, the README must be IGNORED (not kept, not
        // deleted), and a real backup must NOT be deleted to make room for it.
        let input = vec![
            cand("dump-A", 400, 6_000_000), // newest
            cand("README", 300, 0),         // 2nd newest, JUNK
            cand("dump-B", 200, 6_000_000),
            cand("dump-C", 100, 6_000_000), // oldest
        ];
        let qualify = QualifySpec {
            min_size_bytes: Some(1024 * 1024),
            min_age: None,
        };
        let plan = plan_retain_newest(&input, 2, &qualify, None, now());
        assert_partitions(&input, &plan);

        // README is ignored — neither kept nor deleted.
        assert_eq!(plan.ignored.len(), 1);
        assert_eq!(plan.ignored[0].0.key, "README");
        assert_eq!(plan.ignored[0].1, IneligibleReason::BelowMinSize);

        // The two newest QUALIFYING dumps are kept; README did NOT take a slot.
        assert_eq!(keys(&plan.keep), vec!["dump-A", "dump-B"]);
        // Only the genuinely-oldest real dump is deleted.
        assert_eq!(keys(&plan.delete), vec!["dump-C"]);
    }

    #[test]
    fn retain_newest_all_junk_is_safe_noop_never_deletes_reals() {
        // Adversarial: a prefix of nothing but sub-threshold files. The keep set
        // would be junk under a naive impl — here every object is ignored and
        // NOTHING is deleted. "keep junk, delete reals" is impossible by
        // construction because junk never enters the ranking.
        let input = vec![cand("a", 100, 0), cand("b", 200, 10), cand("c", 300, 500)];
        let qualify = QualifySpec {
            min_size_bytes: Some(1024 * 1024),
            min_age: None,
        };
        let plan = plan_retain_newest(&input, 2, &qualify, None, now());
        assert_partitions(&input, &plan);
        assert!(plan.keep.is_empty());
        assert!(plan.delete.is_empty(), "must never delete when all ignored");
        assert_eq!(plan.ignored.len(), 3);
    }

    #[test]
    fn retain_newest_min_age_ignores_too_young_objects() {
        // An object younger than min_age is not yet eligible to count — ignored,
        // not deleted, even though it's outside the newest-K by timestamp.
        let input = vec![
            cand("old1", NOW - 1_000_000, 10), // very old
            cand("old2", NOW - 900_000, 10),
            cand("fresh", NOW - 10, 10), // seconds old
        ];
        let qualify = QualifySpec {
            min_size_bytes: None,
            min_age: Some(chrono::Duration::seconds(3600)),
        };
        // count 1: of the ELIGIBLE (old1, old2), keep the newest (old2), delete old1.
        let plan = plan_retain_newest(&input, 1, &qualify, None, now());
        assert_partitions(&input, &plan);
        assert_eq!(plan.ignored.len(), 1);
        assert_eq!(plan.ignored[0].0.key, "fresh");
        assert_eq!(plan.ignored[0].1, IneligibleReason::BelowMinAge);
        assert_eq!(keys(&plan.keep), vec!["old2"]);
        assert_eq!(keys(&plan.delete), vec!["old1"]);
    }

    #[test]
    fn retain_newest_protect_guard_spares_young_deletions_without_keeping_them() {
        // Eligible-but-young objects outside the keep set are PROTECTED (spared
        // this run), distinct from being KEPT. count 1, protect anything < 1h.
        let input = vec![
            cand("newest", NOW - 10, 10),      // kept (rank 0)
            cand("young", NOW - 100, 10),      // rank 1, young → protected
            cand("oldish", NOW - 100_000, 10), // rank 2, old → deleted
        ];
        let plan = plan_retain_newest(
            &input,
            1,
            &QualifySpec::default(),
            Some(chrono::Duration::seconds(3600)),
            now(),
        );
        assert_partitions(&input, &plan);
        assert_eq!(keys(&plan.keep), vec!["newest"]);
        assert_eq!(keys(&plan.protected), vec!["young"]);
        assert_eq!(keys(&plan.delete), vec!["oldish"]);
    }

    #[test]
    fn retain_newest_qualify_and_protect_are_independent() {
        // A candidate can be ELIGIBLE (passes qualify) yet PROTECTED (guard spares
        // it): it must be neither ignored nor deleted nor kept.
        let input = vec![
            cand("keep1", NOW - 10, 5_000_000),
            cand("keep2", NOW - 20, 5_000_000),
            cand("young_big", NOW - 30, 5_000_000), // eligible, but young → protected
        ];
        let qualify = QualifySpec {
            min_size_bytes: Some(1024),
            min_age: None,
        };
        let plan = plan_retain_newest(
            &input,
            2,
            &qualify,
            Some(chrono::Duration::seconds(3600)),
            now(),
        );
        assert_partitions(&input, &plan);
        assert_eq!(keys(&plan.keep), vec!["keep1", "keep2"]);
        assert_eq!(keys(&plan.protected), vec!["young_big"]);
        assert!(plan.delete.is_empty());
        assert!(plan.ignored.is_empty());
    }

    #[test]
    fn retain_newest_min_size_zero_is_a_noop_filter() {
        // min_size_bytes: Some(0) must admit everything (a 0-byte file has size
        // >= 0), i.e. behave like no filter — guards an off-by-one in `<`.
        let input = vec![cand("a", 100, 0), cand("b", 200, 0)];
        let qualify = QualifySpec {
            min_size_bytes: Some(0),
            min_age: None,
        };
        let plan = plan_retain_newest(&input, 1, &qualify, None, now());
        assert!(plan.ignored.is_empty(), "size>=0 always admits");
        assert_eq!(keys(&plan.keep), vec!["b"]);
        assert_eq!(keys(&plan.delete), vec!["a"]);
    }
}
