//! Pure lifecycle planning functions.

use crate::config_sections::{LifecycleAction, LifecycleRule};
use crate::replication::{normalize_prefix, rewrite_key};
use crate::types::FileMetadata;
use chrono::{DateTime, Utc};
use globset::{Glob, GlobSet, GlobSetBuilder};

pub const MAX_PAGES_PER_RUN: u32 = 10_000;

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
    InvalidGlob { pattern: String, reason: String },
    DestinationRewrite { key: String, reason: String },
    UnsafeSelfMove { bucket: String, key: String },
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
            expire_after: "30d".to_string(),
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
            expire_after: "1d".into(),
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
}
