//! Pure planner functions for replication.
//!
//! All functions in this module are I/O-free: they take typed inputs
//! and return typed outputs. The worker composes them with the engine
//! + state store to perform actual replication.
//!
//! Hot paths (`should_replicate`, `rewrite_key`) run once per listed
//! object so they stay allocation-minimal.

use crate::config_sections::{ConflictPolicy, ReplicationRule};
use crate::types::FileMetadata;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Decision for a single listed object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Copy source → destination (with optional rewritten dest key).
    Copy { dest_key: String },
    /// Skip this object (already in sync, excluded by globs, etc.).
    Skip { reason: SkipReason },
}

/// Why an object was skipped. Surfaced in worker telemetry so
/// operators can diagnose "why didn't X replicate?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Destination has the same-or-newer copy already.
    DestNewerOrEqual,
    /// Rule `conflict: skip-if-dest-exists` and destination exists.
    DestExists,
    /// Object excluded by glob patterns.
    Excluded,
    /// Key is a DeltaGlider internal (reference.bin, .dg/ marker, etc.).
    DgInternal,
    /// Directory marker (zero-byte key ending in `/`). We don't
    /// replicate these — the dest recreates them on-demand.
    DirectoryMarker,
}

/// Plan describing what a single batch iteration should do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BatchPlan {
    /// Objects the worker must copy (source_key, dest_key).
    pub to_copy: Vec<(String, String)>,
    /// Per-object skip reasons — surfaced in totals/telemetry.
    pub skipped: Vec<(String, SkipReason)>,
}

/// Rewrite a source key into the destination's namespace.
///
/// Walks three cases:
/// 1. `source_prefix == dest_prefix` (or both empty): identity.
/// 2. `source_prefix` is a strict prefix of `source_key`: strip, then
///    prepend `dest_prefix`.
/// 3. Otherwise (source_key doesn't start with source_prefix — which
///    shouldn't happen if the engine listing was scoped correctly):
///    return an error so the worker surfaces the bug.
///
/// Returns `Err` only in pathological cases. In normal operation the
/// worker scopes the listing to `source_prefix`, so every key starts
/// with it.
pub fn rewrite_key(
    source_prefix: &str,
    dest_prefix: &str,
    source_key: &str,
) -> Result<String, PlanError> {
    if source_prefix.is_empty() && dest_prefix.is_empty() {
        return Ok(source_key.to_string());
    }
    if source_prefix == dest_prefix {
        // Prefix-swap that happens to be a no-op.
        return Ok(source_key.to_string());
    }
    if source_prefix.is_empty() {
        return Ok(format!("{}{}", dest_prefix, source_key));
    }
    match source_key.strip_prefix(source_prefix) {
        Some(tail) => Ok(format!("{}{}", dest_prefix, tail)),
        None => Err(PlanError::KeyOutsideSourcePrefix {
            key: source_key.to_string(),
            prefix: source_prefix.to_string(),
        }),
    }
}

/// Errors from the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// Listed key doesn't start with the expected source prefix.
    /// Indicates an engine-listing bug; the worker logs and skips.
    KeyOutsideSourcePrefix { key: String, prefix: String },
    /// One of the globs in `include_globs`/`exclude_globs` failed to
    /// compile. Should've been caught at `Config::check` time; kept
    /// as a defensive error for dynamic-config paths.
    InvalidGlob { pattern: String, reason: String },
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::KeyOutsideSourcePrefix { key, prefix } => write!(
                f,
                "source key {:?} does not start with expected source prefix {:?}",
                key, prefix
            ),
            PlanError::InvalidGlob { pattern, reason } => {
                write!(f, "invalid glob {:?}: {}", pattern, reason)
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// Build a combined include/exclude GlobSet for a rule.
///
/// - Returns `(include, exclude)` globsets. Empty include = "include
///   everything" (only excludes apply).
/// - Compiles lazily per planning operation; callers should reuse
///   across batches where possible.
pub fn compile_rule_globs(rule: &ReplicationRule) -> Result<(GlobSet, GlobSet), PlanError> {
    Ok((
        build_globset(&rule.include_globs)?,
        build_globset(&rule.exclude_globs)?,
    ))
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, PlanError> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        match Glob::new(p) {
            Ok(g) => {
                builder.add(g);
            }
            Err(e) => {
                return Err(PlanError::InvalidGlob {
                    pattern: p.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }
    builder.build().map_err(|e| PlanError::InvalidGlob {
        pattern: "<set>".to_string(),
        reason: e.to_string(),
    })
}

/// Pure predicate: should we copy `source_key` from `src_meta` to
/// destination, given optional `dest_meta`?
///
/// The globs must be pre-compiled (cheap if cached across a batch).
pub fn should_replicate(
    source_key: &str,
    src_meta: &FileMetadata,
    dest_meta: Option<&FileMetadata>,
    conflict: ConflictPolicy,
    include_globs: &GlobSet,
    exclude_globs: &GlobSet,
) -> Decision {
    // Directory markers: never replicate. Destination will recreate
    // them on-demand when a nested key lands.
    if source_key.ends_with('/') {
        return Decision::Skip {
            reason: SkipReason::DirectoryMarker,
        };
    }

    // DeltaGlider internals: never leak across replication.
    if source_key.starts_with(".dg/") || source_key.contains("/.dg/") {
        return Decision::Skip {
            reason: SkipReason::DgInternal,
        };
    }

    if exclude_globs.is_match(source_key) {
        return Decision::Skip {
            reason: SkipReason::Excluded,
        };
    }
    if !include_globs.is_empty() && !include_globs.is_match(source_key) {
        return Decision::Skip {
            reason: SkipReason::Excluded,
        };
    }

    match (dest_meta, conflict) {
        // Destination missing — always copy.
        (None, _) => Decision::Copy {
            dest_key: source_key.to_string(),
        },
        // Dest exists + skip-if-dest-exists.
        (Some(_), ConflictPolicy::SkipIfDestExists) => Decision::Skip {
            reason: SkipReason::DestExists,
        },
        // source-wins: always copy.
        (Some(_), ConflictPolicy::SourceWins) => Decision::Copy {
            dest_key: source_key.to_string(),
        },
        // newer-wins: strict comparison on created_at. On ties, fall
        // through to skip (can't distinguish clocks across storage
        // tiers; at-least-once semantics make this safe — next tick
        // will try again if either side's clock advances).
        (Some(dest), ConflictPolicy::NewerWins) => {
            if src_meta.created_at > dest.created_at {
                Decision::Copy {
                    dest_key: source_key.to_string(),
                }
            } else {
                Decision::Skip {
                    reason: SkipReason::DestNewerOrEqual,
                }
            }
        }
    }
}

/// Build a batch plan from a listing page + a per-key destination
/// lookup closure.
///
/// The closure lets callers supply either a pre-built map (tests) or
/// an async HEAD-object call against the engine (worker). Pure
/// functions don't hold an engine reference, so we invert the
/// dependency.
pub async fn plan_batch<F, Fut>(
    objects: &[(String, FileMetadata)],
    rule: &ReplicationRule,
    head_dest: F,
) -> Result<BatchPlan, PlanError>
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = Option<FileMetadata>>,
{
    let (includes, excludes) = compile_rule_globs(rule)?;
    let mut plan = BatchPlan::default();
    for (src_key, src_meta) in objects {
        let dest_key = rewrite_key(&rule.source.prefix, &rule.destination.prefix, src_key)?;
        let dest_meta = head_dest(&dest_key).await;
        match should_replicate(
            src_key,
            src_meta,
            dest_meta.as_ref(),
            rule.conflict,
            &includes,
            &excludes,
        ) {
            Decision::Copy { .. } => {
                plan.to_copy.push((src_key.clone(), dest_key));
            }
            Decision::Skip { reason } => {
                plan.skipped.push((src_key.clone(), reason));
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_sections::{ConflictPolicy, ReplicationEndpoint, ReplicationRule};
    use chrono::{TimeZone, Utc};

    fn make_meta(name: &str, size: u64, ts: chrono::DateTime<Utc>) -> FileMetadata {
        let mut m = FileMetadata::new_passthrough(
            name.to_string(),
            "sha".to_string(),
            format!("{:032}", 0),
            size,
            None,
        );
        m.created_at = ts;
        m
    }

    fn default_rule() -> ReplicationRule {
        ReplicationRule {
            name: "r".to_string(),
            enabled: true,
            source: ReplicationEndpoint {
                bucket: "a".into(),
                prefix: String::new(),
            },
            destination: ReplicationEndpoint {
                bucket: "b".into(),
                prefix: String::new(),
            },
            interval: "1h".into(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::NewerWins,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".into()],
        }
    }

    #[test]
    fn rewrite_key_identity_when_prefixes_empty() {
        let out = rewrite_key("", "", "a/b/c.txt").unwrap();
        assert_eq!(out, "a/b/c.txt");
    }

    #[test]
    fn rewrite_key_strips_source_prefix_and_prepends_dest() {
        let out = rewrite_key("releases/", "archive/2026/", "releases/v1.zip").unwrap();
        assert_eq!(out, "archive/2026/v1.zip");
    }

    #[test]
    fn rewrite_key_same_prefix_is_identity() {
        let out = rewrite_key("pfx/", "pfx/", "pfx/file.txt").unwrap();
        assert_eq!(out, "pfx/file.txt");
    }

    #[test]
    fn rewrite_key_empty_source_prefix_prepends_dest() {
        let out = rewrite_key("", "archive/", "file.txt").unwrap();
        assert_eq!(out, "archive/file.txt");
    }

    #[test]
    fn rewrite_key_errors_on_key_outside_source_prefix() {
        let err = rewrite_key("releases/", "archive/", "something-else/v1.zip").unwrap_err();
        assert!(matches!(err, PlanError::KeyOutsideSourcePrefix { .. }));
    }

    #[test]
    fn should_replicate_missing_dest_copies() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(0, 0).unwrap());
        let d = should_replicate("file.txt", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(d, Decision::Copy { .. }));
    }

    #[test]
    fn should_replicate_newer_wins_skips_when_dest_newer() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(200, 0).unwrap());
        let d = should_replicate("file.txt", &src, Some(&dst), rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::DestNewerOrEqual
            }
        ));
    }

    #[test]
    fn should_replicate_newer_wins_copies_when_source_newer() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(300, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let d = should_replicate("file.txt", &src, Some(&dst), rule.conflict, &inc, &exc);
        assert!(matches!(d, Decision::Copy { .. }));
    }

    #[test]
    fn should_replicate_source_wins_always_copies() {
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::SourceWins;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(500, 0).unwrap());
        let d = should_replicate("file.txt", &src, Some(&dst), rule.conflict, &inc, &exc);
        assert!(matches!(d, Decision::Copy { .. }));
    }

    #[test]
    fn should_replicate_skip_if_dest_exists() {
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::SkipIfDestExists;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(300, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let d = should_replicate("file.txt", &src, Some(&dst), rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::DestExists
            }
        ));
    }

    #[test]
    fn should_replicate_excludes_dg_internals() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("x", 1, Utc.timestamp_opt(0, 0).unwrap());
        let d = should_replicate(".dg/reference.bin", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::DgInternal
            }
        ));
    }

    #[test]
    fn should_replicate_skips_directory_markers() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("x", 0, Utc.timestamp_opt(0, 0).unwrap());
        let d = should_replicate("folder/", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::DirectoryMarker
            }
        ));
    }

    #[test]
    fn should_replicate_honors_exclude_globs() {
        let mut rule = default_rule();
        rule.exclude_globs.push("*.tmp".into());
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("scratch.tmp", 1, Utc.timestamp_opt(0, 0).unwrap());
        let d = should_replicate("scratch.tmp", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::Excluded
            }
        ));
    }

    #[test]
    fn should_replicate_honors_include_globs() {
        let mut rule = default_rule();
        rule.include_globs.push("releases/*".into());
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("x", 1, Utc.timestamp_opt(0, 0).unwrap());
        // In includes → copy.
        let d = should_replicate("releases/v1.zip", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(d, Decision::Copy { .. }));
        // Outside includes → skip.
        let d = should_replicate("staging/v1.zip", &src, None, rule.conflict, &inc, &exc);
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::Excluded
            }
        ));
    }

    #[tokio::test]
    async fn plan_batch_segregates_copy_and_skip() {
        let rule = default_rule();
        let now = Utc.timestamp_opt(500, 0).unwrap();
        let old = Utc.timestamp_opt(100, 0).unwrap();
        let objects = vec![
            ("a.txt".to_string(), make_meta("a.txt", 1, now)),
            ("b.txt".to_string(), make_meta("b.txt", 1, now)),
            ("c.txt".to_string(), make_meta("c.txt", 1, now)),
            (".dg/skip".to_string(), make_meta("skip", 1, now)),
        ];
        // Destination has a.txt newer, b.txt older, c.txt missing.
        let head_dest = |key: &str| {
            let newer = Utc.timestamp_opt(1000, 0).unwrap();
            let out = match key {
                "a.txt" => Some(make_meta(key, 1, newer)),
                "b.txt" => Some(make_meta(key, 1, old)),
                _ => None,
            };
            async move { out }
        };
        let plan = plan_batch(&objects, &rule, head_dest).await.unwrap();
        assert_eq!(plan.to_copy.len(), 2, "b + c copy; a is newer on dest");
        assert!(plan.to_copy.iter().any(|(s, _)| s == "b.txt"));
        assert!(plan.to_copy.iter().any(|(s, _)| s == "c.txt"));
        assert_eq!(plan.skipped.len(), 2, "a skipped + .dg/skip skipped");
    }
}
