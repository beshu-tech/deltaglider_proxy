// SPDX-License-Identifier: GPL-3.0-only

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
    /// `conflict: content-diff` and the destination is byte-identical
    /// (same size, and matching SHA-256 when both sides carry one).
    DestIdentical,
    /// Rule `conflict: skip-if-dest-exists` and destination exists.
    DestExists,
    /// Object excluded by glob patterns.
    Excluded,
    /// Key is a DeltaGlider-managed internal (`.deltaglider/`).
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
    let source_prefix = normalize_prefix(source_prefix);
    let dest_prefix = normalize_prefix(dest_prefix);
    let source_prefix = source_prefix.as_str();
    let dest_prefix = dest_prefix.as_str();

    if source_prefix.is_empty() && dest_prefix.is_empty() {
        return Ok(source_key.to_string());
    }
    if source_prefix == dest_prefix {
        // Prefix-swap that happens to be a no-op.
        return Ok(source_key.to_string());
    }
    if source_prefix.is_empty() {
        return Ok(format!(
            "{}{}",
            dest_prefix,
            source_key.trim_start_matches('/')
        ));
    }
    match source_key.strip_prefix(source_prefix) {
        Some(tail) => Ok(format!("{}{}", dest_prefix, tail.trim_start_matches('/'))),
        None => Err(PlanError::KeyOutsideSourcePrefix {
            key: source_key.to_string(),
            prefix: source_prefix.to_string(),
        }),
    }
}

/// Canonical object-prefix form used by replication:
/// - empty stays empty (whole bucket)
/// - leading slashes are removed
/// - duplicate internal slashes collapse
/// - non-empty prefixes end in exactly one `/`
pub fn normalize_prefix(prefix: &str) -> String {
    let parts: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}/", parts.join("/"))
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

/// Resolve an object's comparable ETag + multipart part-count from its metadata,
/// the same way `parity::ObjState::from_metadata` does: prefer the multipart
/// ETag, else a non-empty md5; parse the `-N` part count off the RESOLVED etag
/// so a foreign multipart object (shape arriving via md5) is handled too.
/// Used by content-diff's tier-2 compare so it agrees with parity/Verify.
fn resolve_etag_and_parts(m: &FileMetadata) -> (Option<String>, Option<u32>) {
    let etag = m
        .multipart_etag
        .clone()
        .or_else(|| (!m.md5.is_empty()).then(|| m.md5.clone()));
    let parts = etag
        .as_deref()
        .and_then(|e| e.rsplit_once('-'))
        .and_then(|(_, n)| n.parse::<u32>().ok());
    (etag, parts)
}

/// PURE: for two same-size objects, do we hold ANY hash that could prove them
/// equal-or-different? True when both carry a SHA-256, OR both carry a
/// comparable etag (same multipart part-count + both present). False → the
/// size-tie is INDETERMINATE (the `strict_content_diff` copy trigger).
fn have_comparable_hash(
    src: &FileMetadata,
    dest: &FileMetadata,
    s_parts: Option<u32>,
    d_parts: Option<u32>,
    s_etag: &Option<String>,
    d_etag: &Option<String>,
) -> bool {
    let both_sha = !src.file_sha256.is_empty() && !dest.file_sha256.is_empty();
    let both_etag = s_parts == d_parts && s_etag.is_some() && d_etag.is_some();
    both_sha || both_etag
}

/// Pure predicate: should we copy `source_key` from `src_meta` to
/// destination, given optional `dest_meta`?
///
/// The globs must be pre-compiled (cheap if cached across a batch).
#[allow(clippy::too_many_arguments)]
pub fn should_replicate(
    source_key: &str,
    src_meta: &FileMetadata,
    dest_meta: Option<&FileMetadata>,
    conflict: ConflictPolicy,
    strict_content_diff: bool,
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

    // DeltaGlider config-sync internals: never leak across replication.
    // Storage-layer delta artifacts (`reference.bin`, `*.delta`) are
    // filtered before planner input by the engine listing.
    if source_key.starts_with(".deltaglider/") || source_key.contains("/.deltaglider/") {
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
        // content-diff: copy only when the bytes actually differ — the
        // converging "exact mirror" policy. Same comparison parity/Verify
        // uses (size is authoritative; SHA-256 confirms equal-size objects
        // when both sides carry one). Byte-identical → skip, so the rule
        // converges. A foreign object missing its SHA on either side falls
        // back to size-only here (can't prove a same-size difference); that
        // matches parity's tier-3 and avoids re-shipping on every sweep.
        (Some(dest), ConflictPolicy::ContentDiff) => {
            let size_differs = src_meta.file_size != dest.file_size;
            let sha_differs = !src_meta.file_sha256.is_empty()
                && !dest.file_sha256.is_empty()
                && src_meta.file_sha256 != dest.file_sha256;
            // Tier-2 (sha absent a side): equal-size objects whose ETags differ
            // ARE different bytes — copy. Mirrors parity::compare_pair so
            // content-diff and Verify agree for foreign/legacy objects (those
            // missing a logical SHA). Only comparable when the multipart shapes
            // match (a multipart `-N` etag is md5-of-md5s, not the object md5);
            // mismatched shapes demote to size-only, like parity's tier-3.
            let (s_etag, s_parts) = resolve_etag_and_parts(src_meta);
            let (d_etag, d_parts) = resolve_etag_and_parts(dest);
            let etag_differs = !sha_differs
                && s_parts == d_parts
                && matches!((&s_etag, &d_etag), (Some(s), Some(d)) if s != d);
            // Same-size objects we CANNOT prove identical: no comparable SHA
            // (either side absent) AND no comparable etag (shape mismatch or
            // absent). Under `strict_content_diff` these resolve to COPY
            // (correctness) rather than the default size-only SKIP (convergence).
            let indeterminate = !size_differs
                && !sha_differs
                && !etag_differs
                && !have_comparable_hash(src_meta, dest, s_parts, d_parts, &s_etag, &d_etag);
            if size_differs || sha_differs || etag_differs || (strict_content_diff && indeterminate)
            {
                Decision::Copy {
                    dest_key: source_key.to_string(),
                }
            } else {
                Decision::Skip {
                    reason: SkipReason::DestIdentical,
                }
            }
        }
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
            rule.strict_content_diff,
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
            strict_content_diff: false,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".into()],
        }
    }

    #[test]
    fn content_diff_size_tie_no_hash_skips_by_default_copies_when_strict() {
        // Both sides same size, NO comparable hash (empty sha, empty md5) →
        // indeterminate. Default: skip (convergence). strict: copy (safety).
        let mut src = make_meta("f.txt", 100, Utc.timestamp_opt(0, 0).unwrap());
        let mut dst = make_meta("f.txt", 100, Utc.timestamp_opt(0, 0).unwrap());
        for m in [&mut src, &mut dst] {
            m.file_sha256 = String::new();
            m.md5 = String::new();
            m.multipart_etag = None;
        }
        let (inc, exc) = (GlobSet::empty(), GlobSet::empty());
        assert!(matches!(
            should_replicate(
                "f.txt",
                &src,
                Some(&dst),
                ConflictPolicy::ContentDiff,
                false,
                &inc,
                &exc
            ),
            Decision::Skip {
                reason: SkipReason::DestIdentical
            }
        ));
        assert!(matches!(
            should_replicate(
                "f.txt",
                &src,
                Some(&dst),
                ConflictPolicy::ContentDiff,
                true,
                &inc,
                &exc
            ),
            Decision::Copy { .. }
        ));
        // With a comparable sha both sides, strict does NOT force-copy identical bytes.
        src.file_sha256 = "abc".into();
        dst.file_sha256 = "abc".into();
        assert!(matches!(
            should_replicate(
                "f.txt",
                &src,
                Some(&dst),
                ConflictPolicy::ContentDiff,
                true,
                &inc,
                &exc
            ),
            Decision::Skip {
                reason: SkipReason::DestIdentical
            }
        ));
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
    fn rewrite_key_normalizes_slashy_prefixes() {
        let out = rewrite_key("/ror/builds//", "/lol//", "ror/builds//free/app.zip").unwrap();
        assert_eq!(out, "lol/free/app.zip");
    }

    #[test]
    fn normalize_prefix_removes_boundary_ambiguity() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("ror/builds"), "ror/builds/");
        assert_eq!(normalize_prefix("/ror//builds//"), "ror/builds/");
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
        let d = should_replicate("file.txt", &src, None, rule.conflict, false, &inc, &exc);
        assert!(matches!(d, Decision::Copy { .. }));
    }

    #[test]
    fn should_replicate_newer_wins_skips_when_dest_newer() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(200, 0).unwrap());
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
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
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(matches!(d, Decision::Copy { .. }));
    }

    #[test]
    fn should_replicate_content_diff_skips_identical_object() {
        // The convergence property that source-wins lacked: identical bytes
        // (same size + same sha) are SKIPPED regardless of timestamps, so a
        // recurring rule eventually copies nothing.
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        // Identical content, but the source timestamp is NEWER (would have
        // copied under newer-wins / source-wins) — content-diff still skips.
        let src = make_meta("file.txt", 10, Utc.timestamp_opt(900, 0).unwrap());
        let dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(
                d,
                Decision::Skip {
                    reason: SkipReason::DestIdentical
                }
            ),
            "identical content must skip, got {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_copies_on_size_difference() {
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 20, Utc.timestamp_opt(100, 0).unwrap());
        let dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(d, Decision::Copy { .. }),
            "size diff must copy: {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_copies_on_sha_difference() {
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        // Same size, but the logical SHA-256 differs on both sides present.
        let mut src = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let mut dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        src.file_sha256 = "aaaa".to_string();
        dst.file_sha256 = "bbbb".to_string();
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(d, Decision::Copy { .. }),
            "sha diff must copy: {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_size_match_missing_sha_skips() {
        // A foreign object whose SHA is absent on one side: can't prove a
        // same-size difference, so fall back to size-only and SKIP (matches
        // parity tier-3; avoids re-shipping every sweep).
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let mut src = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let mut dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        src.file_sha256 = String::new(); // foreign: no logical sha
        dst.file_sha256 = "bbbb".to_string();
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(
                d,
                Decision::Skip {
                    reason: SkipReason::DestIdentical
                }
            ),
            "size-match + missing sha must skip, got {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_foreign_etag_differs_copies() {
        // C4: a foreign object (no logical sha either side), equal size, whose
        // ETag (md5) differs → the bytes differ → COPY. Parity flags this as a
        // tier-2 mismatch; content-diff must agree (it used to wrongly skip).
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let mut src = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let mut dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        src.file_sha256 = String::new();
        dst.file_sha256 = String::new();
        src.md5 = "aaaaaaaa".to_string();
        dst.md5 = "bbbbbbbb".to_string();
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(d, Decision::Copy { .. }),
            "differing etag must copy: {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_foreign_etag_equal_skips() {
        // Same foreign shape but equal ETag → identical bytes → SKIP (converge).
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let mut src = make_meta("file.txt", 10, Utc.timestamp_opt(900, 0).unwrap());
        let mut dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        src.file_sha256 = String::new();
        dst.file_sha256 = String::new();
        src.md5 = "samesame".to_string();
        dst.md5 = "samesame".to_string();
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(
                d,
                Decision::Skip {
                    reason: SkipReason::DestIdentical
                }
            ),
            "equal etag must skip: {d:?}"
        );
    }

    #[test]
    fn should_replicate_content_diff_multipart_shape_mismatch_skips() {
        // Mismatched multipart shapes (different part counts) make the ETags
        // incomparable → demote to size-only and SKIP, exactly like parity
        // tier-3 (a multipart `-N` etag is md5-of-md5s, not the object md5).
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::ContentDiff;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let mut src = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        let mut dst = make_meta("file.txt", 10, Utc.timestamp_opt(100, 0).unwrap());
        src.file_sha256 = String::new();
        dst.file_sha256 = String::new();
        src.multipart_etag = Some("abc-3".to_string()); // 3 parts
        dst.multipart_etag = Some("def-5".to_string()); // 5 parts
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(
            matches!(
                d,
                Decision::Skip {
                    reason: SkipReason::DestIdentical
                }
            ),
            "incomparable multipart shapes must skip (size-only): {d:?}"
        );
    }

    #[test]
    fn should_replicate_skip_if_dest_exists() {
        let mut rule = default_rule();
        rule.conflict = ConflictPolicy::SkipIfDestExists;
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("file.txt", 1, Utc.timestamp_opt(300, 0).unwrap());
        let dst = make_meta("file.txt", 1, Utc.timestamp_opt(100, 0).unwrap());
        let d = should_replicate(
            "file.txt",
            &src,
            Some(&dst),
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(matches!(
            d,
            Decision::Skip {
                reason: SkipReason::DestExists
            }
        ));
    }

    #[test]
    fn should_replicate_excludes_deltaglider_config_sync_prefix() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("x", 1, Utc.timestamp_opt(0, 0).unwrap());
        for key in [".deltaglider/config.db", "nested/.deltaglider/config.db"] {
            let d = should_replicate(key, &src, None, rule.conflict, false, &inc, &exc);
            assert!(matches!(
                d,
                Decision::Skip {
                    reason: SkipReason::DgInternal
                }
            ));
        }
    }

    #[test]
    fn should_replicate_skips_directory_markers() {
        let rule = default_rule();
        let (inc, exc) = compile_rule_globs(&rule).unwrap();
        let src = make_meta("x", 0, Utc.timestamp_opt(0, 0).unwrap());
        let d = should_replicate("folder/", &src, None, rule.conflict, false, &inc, &exc);
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
        let d = should_replicate("scratch.tmp", &src, None, rule.conflict, false, &inc, &exc);
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
        let d = should_replicate(
            "releases/v1.zip",
            &src,
            None,
            rule.conflict,
            false,
            &inc,
            &exc,
        );
        assert!(matches!(d, Decision::Copy { .. }));
        // Outside includes → skip.
        let d = should_replicate(
            "staging/v1.zip",
            &src,
            None,
            rule.conflict,
            false,
            &inc,
            &exc,
        );
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
