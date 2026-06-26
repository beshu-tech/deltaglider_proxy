// SPDX-License-Identifier: GPL-3.0-only

//! Source↔destination parity audit for a replication rule.
//!
//! Answers the operator question "is my mirror verified identical?" with
//! an explicit verdict instead of inferring it from `status=succeeded`.
//!
//! The work splits into a PURE diff kernel (`compare_pair` / `diff_parity`)
//! and an async driver (`parity_audit`) that lists both sides via the same
//! `Pager` + `engine.list_objects(..., true)` the workers use. Parity is a
//! metadata compare — `FileMetadata.file_sha256` is the LOGICAL hash even
//! for delta-stored objects, so no downloads or reconstruction happen.
//!
//! The one correctness trap: `FileMetadata::fallback()` leaves
//! `file_sha256` empty for any object NOT written through this proxy (raw
//! foreign dest). A naive sha-compare would false-alarm every foreign
//! object, so the verifier degrades through three tiers (see `compare_pair`).

use crate::config_db::ConfigDb;
use crate::config_sections::{ConflictPolicy, ReplicationRule};
use crate::deltaglider::DynEngine;
use crate::types::FileMetadata;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use tracing::warn;

use super::event_consumer;
use super::planner::{
    compile_rule_globs, normalize_prefix, rewrite_key, should_replicate, Decision,
};
use super::remediation::{analyze_finding, FindingFacts, Remediation};
use super::state_store::ObjectFailure;

/// Per-category sample cap surfaced to the UI (exact counts stay unbounded).
pub const SAMPLE_CAP: usize = 100;
/// Hard ceiling on total objects scanned across both sides before we stop
/// and report `truncated=true` (2× usage_scanner's 100k — two prefixes).
pub const MAX_PARITY_OBJECTS: usize = 200_000;
/// Objects per `list_objects` page.
const PAGE_SIZE: u32 = 1000;

/// The comparable shape of one object, distilled from `FileMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjState {
    /// Logical SHA-256, empty-string-collapsed to `None` (foreign objects).
    pub sha256: Option<String>,
    /// Logical (hydrated) size in bytes.
    pub size: u64,
    /// `multipart_etag` if present, else `md5` if present (inline — there is
    /// no `FileMetadata::etag()` accessor).
    pub etag: Option<String>,
    /// Part count parsed off a `"...-N"` multipart ETag, if any.
    pub multipart_parts: Option<u32>,
    /// Object creation time (unix MILLIS) — the age signal for newer-wins
    /// remediation. Millis (not whole seconds) so the s>d / s==d / d>s fork
    /// matches the planner's full-DateTime compare. `compare_pair` ignores it.
    pub created_at: Option<i64>,
    /// `Some(true/false)` once the dest scan resolves rule ownership; `None`
    /// on source entries and until annotated (rule-agnostic at construction).
    pub owned_by_rule: Option<bool>,
}

impl ObjState {
    /// Build from listing metadata. Mirrors the plan's field derivation.
    /// `owned_by_rule` is left `None` here (rule-agnostic) — the dest scan
    /// loop sets it where `rule.name` is in scope.
    pub fn from_metadata(m: &FileMetadata) -> Self {
        let sha256 = (!m.file_sha256.is_empty()).then(|| m.file_sha256.clone());
        let etag = m
            .multipart_etag
            .clone()
            .or_else(|| (!m.md5.is_empty()).then(|| m.md5.clone()));
        // Parse the `-N` part count off the RESOLVED etag (not just
        // multipart_etag) so a FOREIGN multipart object — whose multipart shape
        // arrives via md5, with multipart_etag absent — still demotes the tier-2
        // etag compare to size-only instead of a false ChecksumMismatch.
        let multipart_parts = etag
            .as_deref()
            .and_then(|e| e.rsplit_once('-'))
            .and_then(|(_, n)| n.parse::<u32>().ok());
        ObjState {
            sha256,
            size: m.file_size,
            etag,
            multipart_parts,
            created_at: Some(m.created_at.timestamp_millis()),
            owned_by_rule: None,
        }
    }
}

/// Which evidence proved a `Match` (or failed to, for a mismatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verifier {
    /// Strongest: logical SHA-256 + size compared on both sides.
    Sha256,
    /// ETag + size compared (sha missing a side).
    EtagSize,
    /// Only size was comparable.
    SizeOnly,
}

/// The classification of one key across source and destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Match,
    ChecksumMismatch,
    MissingOnDest,
    OrphanOnDest,
}

/// One per-key finding, carried in the bounded sample vecs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParityFinding {
    pub key: String,
    pub kind: FindingKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verifier: Option<Verifier>,
    pub unverifiable: bool,
    pub detail: String,
    /// Cause + "will re-run help?" + guided fix. `None` until annotated
    /// (the pure `diff_parity` never sets it); a nested object once present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<Remediation>,
}

/// PURE three-tier compare of one source/dest pair (both keys present).
///
/// Returns `(kind, verifier, unverifiable, detail)`:
/// 1. Both sha present → compare sha256 + size (strongest).
/// 2. Sha missing a side but both have an etag AND sizes equal → EtagSize,
///    UNLESS the multipart shapes differ (a `-N` count mismatch, including
///    single-part vs multipart) — etags aren't comparable then → fall to 3.
/// 3. Size only: equal → `Match` + `unverifiable`; differ → `ChecksumMismatch`.
///
/// A size difference is ALWAYS a `ChecksumMismatch` (size is authoritative).
pub fn compare_pair(
    src: &ObjState,
    dst: &ObjState,
) -> (FindingKind, Option<Verifier>, bool, String) {
    // Size differs → authoritative mismatch, regardless of tier.
    if src.size != dst.size {
        return (
            FindingKind::ChecksumMismatch,
            None,
            false,
            format!("size differs (src {} vs dst {})", src.size, dst.size),
        );
    }

    // Tier 1: both sha present.
    if let (Some(s), Some(d)) = (&src.sha256, &dst.sha256) {
        if s == d {
            return (
                FindingKind::Match,
                Some(Verifier::Sha256),
                false,
                "sha256 + size match".to_string(),
            );
        }
        return (
            FindingKind::ChecksumMismatch,
            Some(Verifier::Sha256),
            false,
            "sha256 differs".to_string(),
        );
    }

    // Tier 2: etag + size (sha missing a side). A multipart ETag is md5-of-
    // md5s with a `-N` suffix, NOT the object md5 — so it's only comparable
    // when BOTH sides are the same multipart shape. If the part-counts differ,
    // or one side is multipart and the other isn't, the etags can't prove
    // byte-equality → demote to tier 3 (size-only / unverifiable).
    let parts_conflict = src.multipart_parts != dst.multipart_parts;
    if !parts_conflict {
        if let (Some(se), Some(de)) = (&src.etag, &dst.etag) {
            if se == de {
                return (
                    FindingKind::Match,
                    Some(Verifier::EtagSize),
                    false,
                    "etag + size match".to_string(),
                );
            }
            return (
                FindingKind::ChecksumMismatch,
                Some(Verifier::EtagSize),
                false,
                "etag differs at equal size".to_string(),
            );
        }
    }

    // Tier 3: size only. Equal here (size diff handled above) → Match but
    // unverifiable (we couldn't prove byte-equality).
    (
        FindingKind::Match,
        Some(Verifier::SizeOnly),
        true,
        "matched on size only — write through the proxy for checksum parity".to_string(),
    )
}

/// Exact diff counts plus bounded per-category sample vecs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParityDiff {
    pub matched: u64,
    pub checksum_mismatch: u64,
    pub missing_on_dest: u64,
    pub orphan_on_dest: u64,
    /// `Match`es that were only provable by size (subset of `matched`).
    pub unverifiable: u64,
    pub missing_samples: Vec<ParityFinding>,
    pub orphan_samples: Vec<ParityFinding>,
    pub mismatch_samples: Vec<ParityFinding>,
}

/// PURE merge-walk over two sorted maps (keys in the DEST namespace on both
/// sides — the driver pre-rewrites source keys). Classifies each key once.
pub fn diff_parity(
    source: &BTreeMap<String, ObjState>,
    dest: &BTreeMap<String, ObjState>,
) -> ParityDiff {
    let mut out = ParityDiff::default();
    let mut s = source.iter().peekable();
    let mut d = dest.iter().peekable();

    loop {
        match (s.peek(), d.peek()) {
            (Some((sk, sv)), Some((dk, dv))) => {
                match sk.cmp(dk) {
                    std::cmp::Ordering::Equal => {
                        let (kind, verifier, unverifiable, detail) = compare_pair(sv, dv);
                        match kind {
                            FindingKind::Match => {
                                out.matched += 1;
                                if unverifiable {
                                    out.unverifiable += 1;
                                }
                            }
                            FindingKind::ChecksumMismatch => {
                                out.checksum_mismatch += 1;
                                push_capped(
                                    &mut out.mismatch_samples,
                                    ParityFinding {
                                        key: (*sk).clone(),
                                        kind,
                                        verifier,
                                        unverifiable,
                                        detail,
                                        remediation: None,
                                    },
                                );
                            }
                            // compare_pair never yields a missing/orphan for a present pair.
                            _ => {}
                        }
                        s.next();
                        d.next();
                    }
                    std::cmp::Ordering::Less => {
                        // Key only on source → missing on dest.
                        out.missing_on_dest += 1;
                        push_capped(
                            &mut out.missing_samples,
                            ParityFinding {
                                key: (*sk).clone(),
                                kind: FindingKind::MissingOnDest,
                                verifier: None,
                                unverifiable: false,
                                detail: "present on source, absent on destination".to_string(),
                                remediation: None,
                            },
                        );
                        s.next();
                    }
                    std::cmp::Ordering::Greater => {
                        out.orphan_on_dest += 1;
                        push_capped(
                            &mut out.orphan_samples,
                            ParityFinding {
                                key: (*dk).clone(),
                                kind: FindingKind::OrphanOnDest,
                                verifier: None,
                                unverifiable: false,
                                detail: "present on destination, absent on source".to_string(),
                                remediation: None,
                            },
                        );
                        d.next();
                    }
                }
            }
            (Some((sk, _)), None) => {
                out.missing_on_dest += 1;
                push_capped(
                    &mut out.missing_samples,
                    ParityFinding {
                        key: (*sk).clone(),
                        kind: FindingKind::MissingOnDest,
                        verifier: None,
                        unverifiable: false,
                        detail: "present on source, absent on destination".to_string(),
                        remediation: None,
                    },
                );
                s.next();
            }
            (None, Some((dk, _))) => {
                out.orphan_on_dest += 1;
                push_capped(
                    &mut out.orphan_samples,
                    ParityFinding {
                        key: (*dk).clone(),
                        kind: FindingKind::OrphanOnDest,
                        verifier: None,
                        unverifiable: false,
                        detail: "present on destination, absent on source".to_string(),
                        remediation: None,
                    },
                );
                d.next();
            }
            (None, None) => break,
        }
    }
    out
}

fn push_capped(v: &mut Vec<ParityFinding>, f: ParityFinding) {
    if v.len() < SAMPLE_CAP {
        v.push(f);
    }
}

/// PURE: walk the bounded sample vecs, reconstruct `FindingFacts` per finding
/// from the `source`/`dest` maps + the failure `ledger`, run `analyze_finding`,
/// and store the `Remediation` on each finding. Mutates `diff` in place.
///
/// - Missing: source ts, dst ts `None` (absent on dest).
/// - Mismatch: both timestamps from the present pair.
/// - Orphan: dest ts + `owned_by_rule` (resolved during the dest scan).
/// - Ledger lookup inverts the dest-namespace finding key to the raw source key
///   via `dest_to_source` (the ledger is keyed by the worker's source key).
pub fn annotate_findings(
    diff: &mut ParityDiff,
    source: &BTreeMap<String, ObjState>,
    dest: &BTreeMap<String, ObjState>,
    policy: ConflictPolicy,
    replicate_deletes: bool,
    ledger: &HashMap<String, ObjectFailure>,
    dest_to_source: &HashMap<String, String>,
) {
    // Ledger is source-keyed; findings are dest-keyed. Invert, then look up.
    let ledger_for = |dest_key: &str| -> Option<&ObjectFailure> {
        dest_to_source.get(dest_key).and_then(|sk| ledger.get(sk))
    };
    for f in &mut diff.missing_samples {
        let src = source.get(&f.key);
        let facts = FindingFacts {
            kind: f.kind,
            policy,
            replicate_deletes,
            src_created_at: src.and_then(|s| s.created_at),
            dst_created_at: None,
            dest_owned_by_rule: None,
            ledger: ledger_for(&f.key),
        };
        f.remediation = Some(analyze_finding(&facts));
    }
    for f in &mut diff.mismatch_samples {
        let facts = FindingFacts {
            kind: f.kind,
            policy,
            replicate_deletes,
            src_created_at: source.get(&f.key).and_then(|s| s.created_at),
            dst_created_at: dest.get(&f.key).and_then(|d| d.created_at),
            dest_owned_by_rule: None,
            ledger: ledger_for(&f.key),
        };
        f.remediation = Some(analyze_finding(&facts));
    }
    for f in &mut diff.orphan_samples {
        let dst = dest.get(&f.key);
        let facts = FindingFacts {
            kind: f.kind,
            policy,
            replicate_deletes,
            src_created_at: None,
            dst_created_at: dst.and_then(|d| d.created_at),
            dest_owned_by_rule: dst.and_then(|d| d.owned_by_rule),
            ledger: ledger_for(&f.key),
        };
        f.remediation = Some(analyze_finding(&facts));
    }
}

/// PURE: fold the annotated samples into the sample-scoped `ActionableSummary`.
pub fn fold_actionable(diff: &ParityDiff) -> ActionableSummary {
    use super::remediation::{ReasonCode, RerunVerdict};
    let mut s = ActionableSummary::default();
    let all = diff
        .missing_samples
        .iter()
        .chain(&diff.orphan_samples)
        .chain(&diff.mismatch_samples);
    for f in all {
        let Some(rem) = &f.remediation else { continue };
        match rem.rerun_helps {
            RerunVerdict::Yes => s.rerun_fixes += 1,
            RerunVerdict::Conditional { .. } => s.rerun_conditional += 1,
            RerunVerdict::No { .. } => {
                if rem.reason != ReasonCode::CopyFailing {
                    s.needs_manual += 1;
                }
            }
        }
        if rem.reason == ReasonCode::CopyFailing {
            s.copy_failing += 1;
        }
        if rem.reason == ReasonCode::ForeignOrphan {
            s.foreign_orphans += 1;
        }
    }
    s
}

/// Sample-scoped tally of remediation verdicts across the annotated findings.
/// Bounded by the per-category sample caps — NOT the exact diff totals (those
/// stay in `ParityOutcome`'s count fields).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ActionableSummary {
    /// Re-run will fix it (`RerunVerdict::Yes`).
    pub rerun_fixes: u64,
    /// Re-run's outcome depends on timestamps (`RerunVerdict::Conditional`).
    pub rerun_conditional: u64,
    /// Needs operator action — a `No` verdict that isn't a copy-failure.
    pub needs_manual: u64,
    /// The copy keeps failing (`ReasonCode::CopyFailing`).
    pub copy_failing: u64,
    /// Foreign orphans on the destination (`ReasonCode::ForeignOrphan`).
    pub foreign_orphans: u64,
}

/// The serialized audit verdict consumed by the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct ParityOutcome {
    pub rule_name: String,
    pub source_bucket: String,
    pub dest_bucket: String,
    pub source_objects: u64,
    pub dest_objects: u64,
    pub matched: u64,
    pub missing_on_dest: u64,
    pub orphan_on_dest: u64,
    pub checksum_mismatch: u64,
    pub unverifiable: u64,
    pub truncated: bool,
    /// The signal: strict — `unverifiable` counts against it.
    pub in_sync: bool,
    pub scanned_at: i64,
    /// The rule's conflict policy — sets up WHY the verdicts read as they do.
    pub conflict_policy: ConflictPolicy,
    /// Whether the rule mirrors source deletes to the destination.
    pub replicate_deletes: bool,
    /// Sample-scoped remediation tally (see `ActionableSummary`).
    pub actionable: ActionableSummary,
    pub missing_samples: Vec<ParityFinding>,
    pub orphan_samples: Vec<ParityFinding>,
    pub mismatch_samples: Vec<ParityFinding>,
}

/// True when an object listed under a prefix is an internal/marker key we
/// never replicate (so we never count it as an orphan on the dest side).
fn is_skippable_key(key: &str) -> bool {
    key.ends_with('/') || key.starts_with(".deltaglider/") || key.contains("/.deltaglider/")
}

/// Async driver: list both sides, diff, build the outcome.
///
/// SOURCE is filtered through `should_replicate` so the audit covers
/// EXACTLY what replication acts on; each source key is rewritten into the
/// dest namespace. DEST is listed with the same marker/internal skip (but
/// not the source globs — an excluded-but-present dest object is a genuine
/// orphan). Caps total scanned at `MAX_PARITY_OBJECTS`; `truncated=true`
/// rather than hang for huge buckets.
pub async fn parity_audit(
    engine: &DynEngine,
    rule: &ReplicationRule,
    max_objects: usize,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
) -> Result<ParityOutcome, String> {
    let (inc, exc) = compile_rule_globs(rule).map_err(|e| e.to_string())?;
    let source_prefix = normalize_prefix(&rule.source.prefix);
    let dest_prefix = normalize_prefix(&rule.destination.prefix);

    let mut source: BTreeMap<String, ObjState> = BTreeMap::new();
    let mut dest: BTreeMap<String, ObjState> = BTreeMap::new();
    // Reverse map dest-key → raw source-key, so the failure-ledger join (keyed
    // by the worker's raw source_key) can be looked up from a dest-namespace
    // finding even when source.prefix != destination.prefix.
    let mut dest_to_source: HashMap<String, String> = HashMap::new();
    let mut truncated = false;

    // Each side gets its OWN budget (capped at max_objects) so a balanced large
    // mirror isn't spuriously truncated and a big source can't starve the dest
    // scan into emitting false 'missing' findings.
    let mut src_count: usize = 0;

    // --- Scan SOURCE ---
    let mut pager = crate::job_loop::Pager::fresh();
    'src_pages: while let Some(_page) = pager.begin_page() {
        let page = engine
            .list_objects(
                &rule.source.bucket,
                &source_prefix,
                None,
                PAGE_SIZE,
                pager.token(),
                true,
            )
            .await
            .map_err(|e| format!("list source page failed: {e}"))?;
        for (key, meta) in &page.objects {
            if src_count >= max_objects {
                truncated = true;
                break 'src_pages;
            }
            // Keep only what replication would act on (dir markers, internal
            // keys, glob excludes all filtered here).
            let decision =
                should_replicate(key, meta, None, ConflictPolicy::SourceWins, &inc, &exc);
            if !matches!(decision, Decision::Copy { .. }) {
                continue;
            }
            let dest_key = rewrite_key(&rule.source.prefix, &rule.destination.prefix, key)
                .map_err(|e| e.to_string())?;
            dest_to_source.insert(dest_key.clone(), key.clone());
            source.insert(dest_key, ObjState::from_metadata(meta));
            src_count += 1;
        }
        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break;
        }
    }
    if pager.truncated_by_page_budget() {
        truncated = true;
    }

    // --- Scan DEST ---
    let mut dst_count: usize = 0;
    let mut pager = crate::job_loop::Pager::fresh();
    'dst_pages: while let Some(_page) = pager.begin_page() {
        let page = engine
            .list_objects(
                &rule.destination.bucket,
                &dest_prefix,
                None,
                PAGE_SIZE,
                pager.token(),
                true,
            )
            .await
            .map_err(|e| format!("list dest page failed: {e}"))?;
        for (key, meta) in &page.objects {
            if dst_count >= max_objects {
                truncated = true;
                break 'dst_pages;
            }
            if is_skippable_key(key) {
                continue;
            }
            let mut st = ObjState::from_metadata(meta);
            // Resolve rule ownership now, while rule.name is in scope — the
            // foreign-orphan discriminator for remediation.
            st.owned_by_rule = Some(event_consumer::owned_by_rule(meta, &rule.name));
            dest.insert(key.clone(), st);
            dst_count += 1;
        }
        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break;
        }
    }
    if pager.truncated_by_page_budget() {
        truncated = true;
    }

    if truncated {
        warn!(
            "parity audit for rule '{}' hit the scan cap ({} objects) — result is partial",
            rule.name, max_objects
        );
    }

    let source_objects = source.len() as u64;
    let dest_objects = dest.len() as u64;
    let mut diff = diff_parity(&source, &dest);

    let in_sync = !truncated
        && diff.missing_on_dest == 0
        && diff.orphan_on_dest == 0
        && diff.checksum_mismatch == 0
        && diff.unverifiable == 0;

    // Annotate the bounded samples (≤300 keys) with the causal model. The
    // ledger join is one small `IN (…)` query over exactly those keys; empty
    // when no config DB was passed (still a correct, ledger-less diagnosis).
    // The ledger is keyed by the worker's RAW SOURCE key, but findings carry
    // dest-namespace keys — invert via dest_to_source so the join hits when
    // source.prefix != destination.prefix (orphans have no source key → skip).
    let sample_keys: Vec<&str> = diff
        .missing_samples
        .iter()
        .chain(&diff.orphan_samples)
        .chain(&diff.mismatch_samples)
        .filter_map(|f| dest_to_source.get(&f.key))
        .map(|s| s.as_str())
        .collect();
    // Lock the DB ONLY here, for the synchronous ledger query — never across
    // the listing awaits above (a `&ConfigDb` is `!Send`, so holding one across
    // an await would make this future non-`Send` and unusable as a handler).
    let ledger: HashMap<String, ObjectFailure> = match failures {
        Some(mutex) => {
            let db = mutex.lock().await;
            db.replication_object_failures_for_keys(&rule.name, &sample_keys)
                .unwrap_or_default()
        }
        None => HashMap::new(),
    };
    annotate_findings(
        &mut diff,
        &source,
        &dest,
        rule.conflict,
        rule.replicate_deletes,
        &ledger,
        &dest_to_source,
    );
    let actionable = fold_actionable(&diff);

    Ok(ParityOutcome {
        rule_name: rule.name.clone(),
        source_bucket: rule.source.bucket.clone(),
        dest_bucket: rule.destination.bucket.clone(),
        source_objects,
        dest_objects,
        matched: diff.matched,
        missing_on_dest: diff.missing_on_dest,
        orphan_on_dest: diff.orphan_on_dest,
        checksum_mismatch: diff.checksum_mismatch,
        unverifiable: diff.unverifiable,
        truncated,
        in_sync,
        scanned_at: super::current_unix_seconds(),
        conflict_policy: rule.conflict,
        replicate_deletes: rule.replicate_deletes,
        actionable,
        missing_samples: diff.missing_samples,
        orphan_samples: diff.orphan_samples,
        mismatch_samples: diff.mismatch_samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn st(sha: Option<&str>, size: u64, etag: Option<&str>, parts: Option<u32>) -> ObjState {
        ObjState {
            sha256: sha.map(|s| s.to_string()),
            size,
            etag: etag.map(|s| s.to_string()),
            multipart_parts: parts,
            created_at: None,
            owned_by_rule: None,
        }
    }

    // ─────────────── compare_pair truth table ───────────────

    #[test]
    fn both_sha_equal_is_strong_match() {
        let a = st(Some("abc"), 10, Some("e1"), None);
        let (kind, v, unver, _) = compare_pair(&a, &a);
        assert_eq!(kind, FindingKind::Match);
        assert_eq!(v, Some(Verifier::Sha256));
        assert!(!unver);
    }

    #[test]
    fn sha_differ_is_mismatch() {
        let a = st(Some("abc"), 10, None, None);
        let b = st(Some("xyz"), 10, None, None);
        let (kind, v, _, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::ChecksumMismatch);
        assert_eq!(v, Some(Verifier::Sha256));
    }

    #[test]
    fn size_differ_is_always_mismatch_no_verifier() {
        let a = st(Some("abc"), 10, Some("e1"), None);
        let b = st(Some("abc"), 11, Some("e1"), None);
        let (kind, v, unver, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::ChecksumMismatch);
        assert_eq!(v, None);
        assert!(!unver);
    }

    #[test]
    fn etag_equal_match_when_sha_missing_one_side() {
        // dst is foreign (no sha) but both have a matching etag + equal size.
        let a = st(Some("abc"), 10, Some("etag-1"), None);
        let b = st(None, 10, Some("etag-1"), None);
        let (kind, v, unver, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::Match);
        assert_eq!(v, Some(Verifier::EtagSize));
        assert!(!unver);
    }

    #[test]
    fn etag_differ_at_equal_size_is_mismatch() {
        let a = st(None, 10, Some("etag-a"), None);
        let b = st(None, 10, Some("etag-b"), None);
        let (kind, v, _, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::ChecksumMismatch);
        assert_eq!(v, Some(Verifier::EtagSize));
    }

    #[test]
    fn multipart_partcount_mismatch_demotes_to_size_only() {
        // Same etag string is impossible across differing part counts, but the
        // demotion must fire BEFORE the etag compare: differing parts can't
        // prove equality, so we fall to size-only → unverifiable match.
        let a = st(None, 10, Some("e-2"), Some(2));
        let b = st(None, 10, Some("e-3"), Some(3));
        let (kind, v, unver, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::Match);
        assert_eq!(v, Some(Verifier::SizeOnly));
        assert!(unver);
    }

    #[test]
    fn size_only_match_is_unverifiable() {
        // Both foreign, no etag either → size only.
        let a = st(None, 10, None, None);
        let b = st(None, 10, None, None);
        let (kind, v, unver, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::Match);
        assert_eq!(v, Some(Verifier::SizeOnly));
        assert!(unver);
    }

    #[test]
    fn foreign_empty_sha_both_sides_falls_to_etag_then_size() {
        // Both have etags → etag tier even though both sha empty.
        let a = st(None, 5, Some("z"), None);
        let b = st(None, 5, Some("z"), None);
        let (kind, v, _, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::Match);
        assert_eq!(v, Some(Verifier::EtagSize));
    }

    // ─────────────── diff_parity ───────────────

    fn map(pairs: &[(&str, ObjState)]) -> BTreeMap<String, ObjState> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn diff_all_match() {
        let s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let d = s.clone();
        let r = diff_parity(&s, &d);
        assert_eq!(r.matched, 2);
        assert_eq!(r.missing_on_dest, 0);
        assert_eq!(r.orphan_on_dest, 0);
        assert_eq!(r.checksum_mismatch, 0);
        assert_eq!(r.unverifiable, 0);
    }

    #[test]
    fn diff_one_missing() {
        let s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let d = map(&[("a", st(Some("h"), 1, None, None))]);
        let r = diff_parity(&s, &d);
        assert_eq!(r.matched, 1);
        assert_eq!(r.missing_on_dest, 1);
        assert_eq!(r.missing_samples.len(), 1);
        assert_eq!(r.missing_samples[0].key, "b");
    }

    #[test]
    fn diff_one_orphan() {
        let s = map(&[("a", st(Some("h"), 1, None, None))]);
        let d = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("z", st(Some("h3"), 3, None, None)),
        ]);
        let r = diff_parity(&s, &d);
        assert_eq!(r.matched, 1);
        assert_eq!(r.orphan_on_dest, 1);
        assert_eq!(r.orphan_samples[0].key, "z");
    }

    #[test]
    fn diff_one_mismatch() {
        let s = map(&[("a", st(Some("h"), 1, None, None))]);
        let d = map(&[("a", st(Some("DIFFERENT"), 1, None, None))]);
        let r = diff_parity(&s, &d);
        assert_eq!(r.checksum_mismatch, 1);
        assert_eq!(r.matched, 0);
        assert_eq!(r.mismatch_samples[0].key, "a");
    }

    #[test]
    fn diff_empty_empty() {
        let r = diff_parity(&BTreeMap::new(), &BTreeMap::new());
        assert_eq!(r, ParityDiff::default());
    }

    #[test]
    fn diff_source_empty_all_orphan() {
        let d = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let r = diff_parity(&BTreeMap::new(), &d);
        assert_eq!(r.orphan_on_dest, 2);
        assert_eq!(r.missing_on_dest, 0);
    }

    #[test]
    fn diff_dest_empty_all_missing() {
        let s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let r = diff_parity(&s, &BTreeMap::new());
        assert_eq!(r.missing_on_dest, 2);
        assert_eq!(r.orphan_on_dest, 0);
    }

    #[test]
    fn diff_unverifiable_accounting() {
        // One size-only match (unverifiable), one sha match (verifiable).
        let s = map(&[
            ("a", st(None, 1, None, None)),
            ("b", st(Some("h"), 2, None, None)),
        ]);
        let d = map(&[
            ("a", st(None, 1, None, None)),
            ("b", st(Some("h"), 2, None, None)),
        ]);
        let r = diff_parity(&s, &d);
        assert_eq!(r.matched, 2);
        assert_eq!(r.unverifiable, 1);
    }

    #[test]
    fn diff_sample_caps_at_100() {
        let mut s: BTreeMap<String, ObjState> = BTreeMap::new();
        for i in 0..250 {
            s.insert(format!("k{i:04}"), st(Some("h"), 1, None, None));
        }
        let r = diff_parity(&s, &BTreeMap::new());
        assert_eq!(r.missing_on_dest, 250, "exact count is unbounded");
        assert_eq!(r.missing_samples.len(), SAMPLE_CAP, "samples capped at 100");
    }

    // ─────────────── proptest ───────────────

    fn arb_objstate() -> impl Strategy<Value = ObjState> {
        (
            prop::option::of("[a-f0-9]{4}"),
            0u64..1000,
            prop::option::of("[a-z0-9]{1,5}"),
            prop::option::of(1u32..5),
        )
            .prop_map(|(sha, size, etag, parts)| ObjState {
                sha256: sha,
                size,
                etag,
                multipart_parts: parts,
                created_at: None,
                owned_by_rule: None,
            })
    }

    fn arb_map() -> impl Strategy<Value = BTreeMap<String, ObjState>> {
        prop::collection::btree_map("k[0-9]{1,3}", arb_objstate(), 0..30)
    }

    proptest! {
        #[test]
        fn counts_partition_key_union_exactly_once(s in arb_map(), d in arb_map()) {
            let r = diff_parity(&s, &d);
            // Every key in the union lands in exactly one of: matched+mismatch
            // (intersection), missing (source-only), orphan (dest-only).
            let union: std::collections::BTreeSet<&String> =
                s.keys().chain(d.keys()).collect();
            let intersection = s.keys().filter(|k| d.contains_key(*k)).count() as u64;
            let source_only = s.keys().filter(|k| !d.contains_key(*k)).count() as u64;
            let dest_only = d.keys().filter(|k| !s.contains_key(*k)).count() as u64;

            prop_assert_eq!(r.matched + r.checksum_mismatch, intersection);
            prop_assert_eq!(r.missing_on_dest, source_only);
            prop_assert_eq!(r.orphan_on_dest, dest_only);
            prop_assert_eq!(
                r.matched + r.checksum_mismatch + r.missing_on_dest + r.orphan_on_dest,
                union.len() as u64
            );
            // unverifiable is a subset of matched.
            prop_assert!(r.unverifiable <= r.matched);
        }

        #[test]
        fn samples_never_exceed_cap(s in arb_map(), d in arb_map()) {
            let r = diff_parity(&s, &d);
            prop_assert!(r.missing_samples.len() <= SAMPLE_CAP);
            prop_assert!(r.orphan_samples.len() <= SAMPLE_CAP);
            prop_assert!(r.mismatch_samples.len() <= SAMPLE_CAP);
        }

        #[test]
        fn in_sync_iff_all_zero_and_not_truncated(
            s in arb_map(), d in arb_map(), truncated in any::<bool>()
        ) {
            let r = diff_parity(&s, &d);
            let in_sync = !truncated
                && r.missing_on_dest == 0
                && r.orphan_on_dest == 0
                && r.checksum_mismatch == 0
                && r.unverifiable == 0;
            let all_clean = r.missing_on_dest == 0
                && r.orphan_on_dest == 0
                && r.checksum_mismatch == 0
                && r.unverifiable == 0;
            prop_assert_eq!(in_sync, !truncated && all_clean);
        }
    }

    #[test]
    fn objstate_parses_multipart_part_count() {
        let mut m =
            FileMetadata::new_passthrough("x".into(), "sha".into(), "md5val".into(), 7, None);
        m.multipart_etag = Some("deadbeef-4".to_string());
        let st = ObjState::from_metadata(&m);
        assert_eq!(st.sha256.as_deref(), Some("sha"));
        assert_eq!(st.etag.as_deref(), Some("deadbeef-4"));
        assert_eq!(st.multipart_parts, Some(4));
        assert_eq!(st.size, 7);
    }

    #[test]
    fn objstate_foreign_object_has_no_sha_but_keeps_md5_etag() {
        use crate::types::StorageInfo;
        let m = FileMetadata::fallback(
            "x".into(),
            12,
            "md5val".into(),
            chrono::Utc::now(),
            None,
            StorageInfo::Passthrough,
        );
        let st = ObjState::from_metadata(&m);
        assert_eq!(st.sha256, None);
        assert_eq!(st.etag.as_deref(), Some("md5val"));
        assert_eq!(st.multipart_parts, None);
    }

    #[test]
    fn objstate_carries_created_at_and_no_ownership() {
        let now = chrono::Utc::now();
        let m = FileMetadata::new_passthrough("x".into(), "sha".into(), "md5val".into(), 7, None);
        // new_passthrough stamps created_at = now; assert we propagate it.
        let st = ObjState::from_metadata(&m);
        // Sub-second precision (millis) so the newer-wins fork matches the planner.
        assert_eq!(st.created_at, Some(m.created_at.timestamp_millis()));
        assert!(st.created_at.unwrap() >= now.timestamp_millis() - 5000);
        assert_eq!(st.owned_by_rule, None, "ownership is rule-agnostic here");
    }

    // ─────────────── annotate_findings ───────────────

    #[test]
    fn annotate_missing_no_ledger_is_run_now() {
        use super::super::remediation::{FixAction, ReasonCode, RerunVerdict};
        let mut src = st(Some("h"), 1, None, None);
        src.created_at = Some(500);
        let source = map(&[("k", src)]);
        let dest: BTreeMap<String, ObjState> = BTreeMap::new();
        let mut diff = diff_parity(&source, &dest);
        let d2s = HashMap::from([("k".to_string(), "k".to_string())]);
        annotate_findings(
            &mut diff,
            &source,
            &dest,
            ConflictPolicy::NewerWins,
            false,
            &HashMap::new(),
            &d2s,
        );
        let rem = diff.missing_samples[0].remediation.as_ref().unwrap();
        assert_eq!(rem.reason, ReasonCode::NeverCopied);
        assert_eq!(rem.rerun_helps, RerunVerdict::Yes);
        assert_eq!(rem.fix, FixAction::RunNow);
    }

    #[test]
    fn annotate_skip_mismatch_is_the_lie_and_folds_to_needs_manual() {
        use super::super::remediation::{NoReason, RerunVerdict};
        // Same size, differing sha → mismatch under SkipIfDestExists.
        let mut s = st(Some("AAAA"), 10, None, None);
        s.created_at = Some(100);
        let mut d = st(Some("BBBB"), 10, None, None);
        d.created_at = Some(100);
        d.owned_by_rule = Some(true);
        let source = map(&[("k", s)]);
        let dest = map(&[("k", d)]);
        let mut diff = diff_parity(&source, &dest);
        let d2s = HashMap::from([("k".to_string(), "k".to_string())]);
        annotate_findings(
            &mut diff,
            &source,
            &dest,
            ConflictPolicy::SkipIfDestExists,
            false,
            &HashMap::new(),
            &d2s,
        );
        let rem = diff.mismatch_samples[0].remediation.as_ref().unwrap();
        assert_eq!(
            rem.rerun_helps,
            RerunVerdict::No {
                why: NoReason::PolicySkipsExistingDest
            }
        );
        let summary = fold_actionable(&diff);
        assert_eq!(summary.needs_manual, 1);
        assert_eq!(summary.rerun_fixes, 0);
    }

    #[test]
    fn annotate_orphan_uses_dest_ownership() {
        use super::super::remediation::ReasonCode;
        let source: BTreeMap<String, ObjState> = BTreeMap::new();
        let mut d = st(Some("h"), 5, None, None);
        d.owned_by_rule = Some(false); // foreign
        let dest = map(&[("z", d)]);
        let mut diff = diff_parity(&source, &dest);
        annotate_findings(
            &mut diff,
            &source,
            &dest,
            ConflictPolicy::NewerWins,
            true,
            &HashMap::new(),
            &HashMap::new(),
        );
        let rem = diff.orphan_samples[0].remediation.as_ref().unwrap();
        assert_eq!(rem.reason, ReasonCode::ForeignOrphan);
        assert_eq!(fold_actionable(&diff).foreign_orphans, 1);
    }

    #[test]
    fn ledger_join_inverts_dest_key_to_source_key_across_prefixes() {
        // F1: rule rewrites src "firmware/a.bin" → dest "mirror/a.bin". The
        // failure ledger is keyed by the SOURCE key; the finding by the DEST
        // key. The join must invert via dest_to_source or CopyFailing is lost.
        use super::super::remediation::ReasonCode;
        let mut s = st(Some("h"), 1, None, None);
        s.created_at = Some(500);
        let source = map(&[("mirror/a.bin", s)]); // already dest-namespace in the map
        let dest: BTreeMap<String, ObjState> = BTreeMap::new();
        let mut diff = diff_parity(&source, &dest);
        let ledger = HashMap::from([(
            "firmware/a.bin".to_string(),
            ObjectFailure {
                consecutive_failures: 3,
                last_error: "AccessDenied".to_string(),
                last_failed_at: 1,
            },
        )]);
        let d2s = HashMap::from([("mirror/a.bin".to_string(), "firmware/a.bin".to_string())]);
        annotate_findings(
            &mut diff,
            &source,
            &dest,
            ConflictPolicy::NewerWins,
            false,
            &ledger,
            &d2s,
        );
        // With the inversion the missing object is correctly CopyFailing, NOT
        // the harmful NeverCopied/"re-run fixes this".
        let rem = diff.missing_samples[0].remediation.as_ref().unwrap();
        assert_eq!(rem.reason, ReasonCode::CopyFailing);
    }

    #[test]
    fn foreign_multipart_object_demotes_to_size_only_not_false_mismatch() {
        // F5: a foreign dest object carries its multipart shape in md5 (no
        // multipart_etag). The src is a managed single-part object. Same bytes,
        // different etag SHAPE must NOT report a false ChecksumMismatch.
        let src =
            FileMetadata::new_passthrough("x".into(), String::new(), "abc123".into(), 10, None);
        let mut dst = FileMetadata::fallback(
            "x".into(),
            10,
            "abc123-4".into(), // multipart-shaped md5, 4 parts, foreign (no sha)
            chrono::Utc::now(),
            None,
            crate::types::StorageInfo::Passthrough,
        );
        dst.file_sha256 = String::new();
        let a = ObjState::from_metadata(&src);
        let b = ObjState::from_metadata(&dst);
        // b's multipart_parts must be parsed off the resolved etag (md5 here).
        assert_eq!(b.multipart_parts, Some(4));
        let (kind, v, unver, _) = compare_pair(&a, &b);
        assert_eq!(kind, FindingKind::Match, "must not false-mismatch");
        assert_eq!(v, Some(Verifier::SizeOnly));
        assert!(unver);
    }
}
