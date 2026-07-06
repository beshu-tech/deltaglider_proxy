// SPDX-License-Identifier: GPL-3.0-only

//! Source↔destination parity audit for a replication rule.
//!
//! Answers the operator question "is my mirror verified identical?" with
//! an explicit verdict instead of inferring it from `status=succeeded`.
//!
//! The work splits into a PURE diff kernel (`compare_pair` / `diff_parity`)
//! and an async driver (`parity_audit`) that LITE-lists both sides (no
//! per-object HEAD), then resolves each delta/eligible object's LOGICAL
//! metadata from a persistent per-object cache (`replication_parity_objects`),
//! HEADing only cache misses + changed objects. Parity is a metadata compare —
//! `FileMetadata.file_sha256` is the LOGICAL hash even for delta-stored objects,
//! so no downloads or reconstruction happen. A re-verify is HEAD-free.
//!
//! The one correctness trap: `FileMetadata::fallback()` leaves
//! `file_sha256` empty for any object NOT written through this proxy (raw
//! foreign dest). A naive sha-compare would false-alarm every foreign
//! object, so the verifier degrades through three tiers (see `compare_pair`).

use crate::config_db::ConfigDb;
use crate::config_sections::{ConflictPolicy, ReplicationRule};
use crate::deltaglider::DynEngine;
use crate::types::FileMetadata;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use tracing::warn;

use super::event_consumer;
use super::planner::{
    compile_rule_globs, normalize_prefix, rewrite_key, should_replicate, Decision,
};
use super::remediation::{analyze_finding, FindingFacts, Remediation};
use super::state_store::{ObjectFailure, ParityCacheEntry, ParitySide};

/// Live-progress + cancel control for a background parity audit. The driver
/// reports `objects scanned so far` into the parity row (throttled — every
/// [`PROGRESS_FLUSH_EVERY_N_PAGES`] pages), and checks for cancellation. Cancel uses
/// TWO signals: a fast in-process `AtomicBool` (no lock, checked every page)
/// AND the durable `cancelling` DB row (checked at phase boundaries — covers a
/// cancel from ANOTHER instance / after a restart, where the in-process flag is
/// absent). Passing `None` runs without progress/cancel (no-DB fallback path).
pub struct ParityProgress<'a> {
    pub db: &'a tokio::sync::Mutex<ConfigDb>,
    pub rule: &'a str,
    /// In-process cancel flag set by this instance's `verify_cancel`.
    pub cancel: &'a std::sync::atomic::AtomicBool,
}

/// Flush progress to the DB once every N pages (not every page) — the global
/// ConfigDb mutex is shared with the whole IAM/admin path. At 8 pages (~8k
/// objects) the live count ticks every few seconds so the UI reads as alive and
/// a client-side rate stays smooth; the mutex cost of the extra flushes over a
/// minutes-long scan is negligible.
const PROGRESS_FLUSH_EVERY_N_PAGES: usize = 8;

/// Pure decision: should the per-page progress counter flush to the DB at this
/// page index? Flushes on every `every`-th page (page 0, `every`, `2*every`, …).
/// Extracted so the cadence is unit-testable without a DB / scan.
fn should_flush(page: u64, every: u64) -> bool {
    every > 0 && page.is_multiple_of(every)
}

/// Sentinel error string an audit returns when cancelled, so the caller can
/// settle the row as `cancelled` rather than `failed`. `ponytail`: a sentinel
/// over a custom error enum — the audit's error channel is already `String`,
/// and cancel is the single case that needs distinguishing.
pub const CANCELLED: &str = "__parity_cancelled__";

/// Monotonic counter bumped each time a background parity audit SETTLES
/// (done/failed/cancelled). Mirrors `IAM_VERSION` — lets integration tests poll
/// `GET …/jobs/parity-version` for a deterministic completion barrier instead of
/// sleeping. Process-local (one per proxy), matching one-process-per-TestServer.
static PARITY_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bump on settle. Call AFTER the terminal status row is written so a poller
/// that sees the new version also sees the settled row.
pub fn bump_parity_version() -> u64 {
    PARITY_VERSION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

/// Current settle count.
pub fn current_parity_version() -> u64 {
    PARITY_VERSION.load(std::sync::atomic::Ordering::SeqCst)
}

impl ParityProgress<'_> {
    /// Fast in-process cancel check (no lock) — every page.
    fn cancelled_local(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cancel check at phase boundaries OUTSIDE the per-page loop (the
    /// resolve/HEAD-burst tail can be slow on a cold cache). Checks the
    /// in-process flag first (cheap), then the durable `cancelling` row (covers
    /// a cancel from another instance / after a restart).
    async fn check_cancel(&self) -> Result<(), String> {
        if self.cancelled_local() {
            return Err(CANCELLED.to_string());
        }
        let db = self.db.lock().await;
        if matches!(db.parity_status(self.rule).ok().flatten(), Some(s) if s == "cancelling") {
            return Err(CANCELLED.to_string());
        }
        Ok(())
    }

    /// Publish the compare-phase denominator once listing finishes (best-effort;
    /// a failed write just leaves the bar indeterminate).
    async fn set_total(&self, total: u64) {
        let now = super::current_unix_seconds();
        let db = self.db.lock().await;
        let _ = db.parity_result_set_total(self.rule, total as i64, now);
    }
}

/// Which comparison regime a rule runs in — the plan's core split.
///
/// `PureMirror`: dest is a byte-identical verbatim copy (same compression, both
/// sides' lite list is trustworthy → plaintext, non-encrypting). We compare the
/// STORED blob size+etag straight from the lite list — ZERO HEADs.
///
/// `Transforming`: dest bytes are re-derived (re-encrypt / compression change /
/// re-delta), so stored size legitimately differs; only the logical SHA is
/// faithful → the existing HEAD-burst compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Regime {
    PureMirror,
    Transforming,
}

/// serde default for `ParityOutcome::regime` on pre-field JSON (the safe/strict
/// verdict — logical checksum).
fn default_regime() -> Regime {
    Regime::Transforming
}

/// PURE classifier. `PureMirror` only when BOTH sides carry trustworthy lite
/// facts (plaintext, non-encrypting, ownership-in-list) AND compression matches
/// — else any stored-size/etag compare would false-flag correct mirrors.
/// A single signal (`lite_list_carries_logical_facts`) already subsumes
/// "encrypting" and "S3 without metadata"; compression parity is the one extra.
pub fn classify_regime(
    src_lite_authoritative: bool,
    dst_lite_authoritative: bool,
    src_compression: bool,
    dst_compression: bool,
) -> Regime {
    if src_lite_authoritative && dst_lite_authoritative && src_compression == dst_compression {
        Regime::PureMirror
    } else {
        Regime::Transforming
    }
}

/// Per-category sample cap surfaced to the UI (exact counts stay unbounded).
pub const SAMPLE_CAP: usize = 100;
/// Hard ceiling on total objects scanned across both sides before we stop
/// and report `truncated=true` (2× usage_scanner's 100k — two prefixes).
pub const MAX_PARITY_OBJECTS: usize = 200_000;
/// Objects per `list_objects` page.
const PAGE_SIZE: u32 = 1000;
/// Per-page list retries on a transient throttle (503) before giving up.
const LIST_MAX_ATTEMPTS: u32 = 5;

/// The comparable shape of one object, distilled from `FileMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjState {
    /// Logical SHA-256, empty-string-collapsed to `None` (foreign objects).
    pub sha256: Option<String>,
    /// Logical (hydrated) size in bytes.
    pub size: u64,
    /// STORED (physical) blob size — `delta_size` for a delta, else `file_size`.
    /// Immune to the file_size cache-correction; used ONLY by the PureMirror
    /// compare, where dest bytes are verbatim so stored size+etag IS the proof.
    pub stored_size: u64,
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
            stored_size: m.stored_size(),
            etag,
            multipart_parts,
            created_at: Some(m.created_at.timestamp_millis()),
            owned_by_rule: None,
        }
    }
}

/// Which evidence proved a `Match` (or failed to, for a mismatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Match,
    ChecksumMismatch,
    MissingOnDest,
    OrphanOnDest,
}

/// One per-key finding, carried in the bounded sample vecs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// PURE compare for the `PureMirror` regime: the dest blob is a verbatim copy,
/// so equal STORED size + equal stored etag proves a byte-identical mirror with
/// zero HEADs. (S3 etag = content MD5 for a single-part object.) A stored-size
/// difference means the object was NOT verbatim-shipped (a re-delta) — flag it
/// for a logical HEAD rather than asserting a mismatch.
pub fn compare_pair_stored(
    src: &ObjState,
    dst: &ObjState,
) -> (FindingKind, Option<Verifier>, bool, String) {
    if src.stored_size != dst.stored_size {
        // Not a verbatim copy — caller HEADs this key for a logical verdict.
        return (
            FindingKind::ChecksumMismatch,
            None,
            true,
            format!(
                "stored size differs (src {} vs dst {}) — re-delta, needs checksum",
                src.stored_size, dst.stored_size
            ),
        );
    }
    match (&src.etag, &dst.etag) {
        (Some(se), Some(de)) if se == de => (
            FindingKind::Match,
            Some(Verifier::EtagSize),
            false,
            "exact copy — stored size + etag match".to_string(),
        ),
        (Some(_), Some(_)) => (
            FindingKind::ChecksumMismatch,
            Some(Verifier::EtagSize),
            false,
            "stored etag differs at equal size".to_string(),
        ),
        // Equal stored size, etag missing a side → size-only proof.
        _ => (
            FindingKind::Match,
            Some(Verifier::SizeOnly),
            true,
            "matched on stored size only".to_string(),
        ),
    }
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
    regime: Regime,
) -> ParityDiff {
    let mut out = ParityDiff::default();
    let mut s = source.iter().peekable();
    let mut d = dest.iter().peekable();

    loop {
        match (s.peek(), d.peek()) {
            (Some((sk, sv)), Some((dk, dv))) => {
                match sk.cmp(dk) {
                    std::cmp::Ordering::Equal => {
                        // PureMirror compares STORED size+etag (no HEAD ran);
                        // Transforming compares logical sha/etag/size.
                        let (kind, verifier, unverifiable, detail) = match regime {
                            Regime::PureMirror => compare_pair_stored(sv, dv),
                            Regime::Transforming => compare_pair(sv, dv),
                        };
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// How parity was checked: `pure_mirror` (verbatim copy → stored size+etag,
    /// no downloads) vs `transforming` (re-derived bytes → logical SHA HEAD).
    /// Drives the honest provenance footnote. Defaults to `transforming` for
    /// outcomes serialized before this field existed.
    #[serde(default = "default_regime")]
    pub regime: Regime,
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

/// True when the LITE list entry can't be trusted for parity and a logical
/// resolution (cache or HEAD) is needed: a delta object (lite carries the
/// delta-blob size/etag, not logical) or a delta-ELIGIBLE key (it MIGHT be
/// delta-stored, so the lite size could be the delta size). A non-eligible
/// passthrough object (a `.sha1` sidecar, an image) is stored verbatim — the
/// lite size/etag ARE the truth, so no resolution is needed (the common case).
fn needs_logical_resolution(engine: &DynEngine, key: &str, meta: &FileMetadata) -> bool {
    meta.is_delta() || engine.is_delta_eligible_key(key)
}

/// Overlay logical (sha256, size, etag) onto the `ObjState` in `map` for `key`.
fn apply_logical(map: &mut BTreeMap<String, ObjState>, map_key: &str, e: &ParityCacheEntry) {
    if let Some(st) = map.get_mut(map_key) {
        st.sha256 = e.sha256.clone();
        st.size = e.size;
        st.etag = e.etag.clone();
        st.multipart_parts = e
            .etag
            .as_deref()
            .and_then(|s| s.rsplit_once('-'))
            .and_then(|(_, n)| n.parse::<u32>().ok());
    }
}

/// A logical-metadata cache entry from a fresh HEAD. `stored_etag` is the
/// CONTENT-VERSION token — the etag of the STORED blob (delta-blob for a delta
/// object, the object etag for passthrough), captured from the lite list at
/// resolve time and stamped here so the next verify can detect an overwrite.
fn cache_entry_from_meta(m: &FileMetadata, stored_etag: Option<String>) -> ParityCacheEntry {
    let sha256 = (!m.file_sha256.is_empty()).then(|| m.file_sha256.clone());
    let etag = m
        .multipart_etag
        .clone()
        .or_else(|| (!m.md5.is_empty()).then(|| m.md5.clone()));
    ParityCacheEntry {
        sha256,
        size: m.file_size,
        etag,
        stored_etag,
    }
}

/// The STORED-blob etag the lite list recorded for `map_key` (the content-version
/// token). Read from the ObjState BEFORE any logical overlay.
fn lite_stored_etag(map: &BTreeMap<String, ObjState>, map_key: &str) -> Option<String> {
    map.get(map_key).and_then(|st| st.etag.clone())
}

/// A cache hit is only valid when the stored blob hasn't changed since it was
/// cached: the cached `stored_etag` must equal the current lite `stored_etag`.
/// A `None`/`None` pair (no etag either side) is treated as a MISS — we can't
/// prove the object is unchanged, so we re-read rather than risk a stale verdict.
fn cache_hit_fresh(cached: &ParityCacheEntry, lite_stored_etag: &Option<String>) -> bool {
    matches!((&cached.stored_etag, lite_stored_etag), (Some(a), Some(b)) if a == b)
}

/// Resolve logical metadata for SOURCE keys queued for resolution: parity cache
/// first (HEAD-free, but ONLY when the stored-etag still matches), then a bounded
/// HEAD burst for the misses + the changed objects, persisting fresh results so
/// the next verify is HEAD-free. `source` is keyed by the dest-namespace key.
///
/// Returns the number of keys left UNRESOLVED by a transient HEAD failure —
/// the caller must treat a non-zero count as a partial audit (in_sync=false),
/// since a dropped key can't be compared and would otherwise vanish silently.
#[allow(clippy::too_many_arguments)]
async fn resolve_logical(
    engine: &DynEngine,
    rule: &ReplicationRule,
    bucket: &str,
    src_prefix: &str,
    dst_prefix: &str,
    raw_keys: &[String],
    source: &mut BTreeMap<String, ObjState>,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
) -> usize {
    if raw_keys.is_empty() {
        return 0;
    }
    let dest_keys: Vec<String> = raw_keys
        .iter()
        .filter_map(|k| rewrite_key(src_prefix, dst_prefix, k).ok())
        .collect();
    let cached = cache_get(failures, &rule.name, ParitySide::Source, &dest_keys).await;
    // Trust a cache hit ONLY if the stored blob is unchanged; else HEAD it.
    let mut miss_raw: Vec<&String> = Vec::new();
    for raw in raw_keys {
        let Ok(dk) = rewrite_key(src_prefix, dst_prefix, raw) else {
            continue;
        };
        let lite = lite_stored_etag(source, &dk);
        match cached.get(&dk) {
            Some(e) if cache_hit_fresh(e, &lite) => apply_logical(source, &dk, e),
            _ => miss_raw.push(raw),
        }
    }
    let fresh = head_burst(engine, bucket, &miss_raw).await;
    let mut to_cache: Vec<(String, ParityCacheEntry)> = Vec::new();
    let mut unresolved = 0usize;
    for (raw, outcome) in fresh {
        let Ok(dk) = rewrite_key(src_prefix, dst_prefix, &raw) else {
            continue;
        };
        match outcome {
            HeadOutcome::Resolved(meta) => {
                let stored = lite_stored_etag(source, &dk);
                let e = cache_entry_from_meta(&meta, stored);
                apply_logical(source, &dk, &e);
                to_cache.push((dk, e));
            }
            // Raced delete → drop from the compare (it's genuinely gone).
            HeadOutcome::Gone => {
                source.remove(&dk);
            }
            // Transient HEAD failure → drop from the compare (a false verdict on
            // the untrusted lite size is worse, #16) but COUNT it so the audit
            // is reported partial, not silently in_sync (#3 regression).
            HeadOutcome::Unresolved => {
                source.remove(&dk);
                unresolved += 1;
            }
        }
    }
    cache_put(failures, &rule.name, ParitySide::Source, &to_cache).await;
    unresolved
}

/// Dest-side logical resolution: dest is keyed by its own raw key (== cache key).
/// Returns the count of keys left UNRESOLVED by a transient HEAD (see
/// `resolve_logical`). A non-zero count makes the audit partial.
async fn resolve_logical_dest(
    engine: &DynEngine,
    rule: &ReplicationRule,
    bucket: &str,
    raw_keys: &[String],
    dest: &mut BTreeMap<String, ObjState>,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
) -> usize {
    if raw_keys.is_empty() {
        return 0;
    }
    let keys: Vec<String> = raw_keys.to_vec();
    // When the lite list can't carry provenance (S3 / encrypting dest),
    // ownership was left provisional in the scan and must be overlaid from a
    // FRESH HEAD. The parity cache stores logical facts (size/etag/sha) but NOT
    // ownership, and ownership isn't stored-etag-stable (it depends on the
    // source still existing), so a cache hit would leave the provisional
    // not-owned in place — re-misdiagnosing a rule-owned orphan as foreign on
    // every verify after the first. So on a non-authoritative dest, bypass the
    // cache entirely: every key gets a fresh HEAD.
    let lite_authoritative = engine.lite_list_carries_logical_facts(bucket);
    let cached = if lite_authoritative {
        cache_get(failures, &rule.name, ParitySide::Dest, &keys).await
    } else {
        std::collections::HashMap::new()
    };
    let mut miss: Vec<&String> = Vec::new();
    for k in raw_keys {
        let lite = lite_stored_etag(dest, k);
        match cached.get(k) {
            Some(e) if cache_hit_fresh(e, &lite) => apply_logical(dest, k, e),
            _ => miss.push(k),
        }
    }
    let fresh = head_burst(engine, bucket, &miss).await;
    let mut to_cache: Vec<(String, ParityCacheEntry)> = Vec::new();
    let mut unresolved = 0usize;
    for (k, outcome) in fresh {
        match outcome {
            HeadOutcome::Resolved(meta) => {
                if !lite_authoritative {
                    if let Some(st) = dest.get_mut(&k) {
                        st.owned_by_rule = Some(event_consumer::owned_by_rule(&meta, &rule.name));
                    }
                }
                let stored = lite_stored_etag(dest, &k);
                let e = cache_entry_from_meta(&meta, stored);
                apply_logical(dest, &k, &e);
                to_cache.push((k, e));
            }
            HeadOutcome::Gone => {
                dest.remove(&k);
            }
            // Transient HEAD failure → drop (a false verdict is worse) but COUNT
            // so the audit is partial — a dropped dest orphan would otherwise
            // vanish and yield a false in_sync (#3 regression).
            HeadOutcome::Unresolved => {
                dest.remove(&k);
                unresolved += 1;
            }
        }
    }
    cache_put(failures, &rule.name, ParitySide::Dest, &to_cache).await;
    unresolved
}

/// Outcome of one HEAD in the burst. A NotFound is a raced delete (the object
/// is genuinely gone → drop it). A TRANSIENT error (throttle/5xx) must NOT be
/// treated as "resolved to the lite/delta size" — that yields a false
/// ChecksumMismatch. It is surfaced as Unresolved so the caller keeps the key
/// out of the compare rather than emitting a bogus verdict (finding #16).
enum HeadOutcome {
    Resolved(Box<FileMetadata>),
    Gone,
    Unresolved,
}

/// Bounded-concurrent HEAD burst (the cache-miss path). Returns per-key
/// outcomes so a transient failure is distinguishable from a raced delete.
async fn head_burst(
    engine: &DynEngine,
    bucket: &str,
    keys: &[&String],
) -> Vec<(String, HeadOutcome)> {
    use futures::stream::StreamExt;
    const HEAD_CONCURRENCY: usize = 50;
    let owned: Vec<String> = keys.iter().map(|k| (*k).clone()).collect();
    futures::stream::iter(owned.into_iter().map(|key| async move {
        let outcome = match engine.head(bucket, &key).await {
            Ok(m) => HeadOutcome::Resolved(Box::new(m)),
            Err(e) => {
                let s = e.to_string();
                let s = s.to_ascii_lowercase();
                if s.contains("not found") || s.contains("nosuchkey") {
                    HeadOutcome::Gone
                } else {
                    HeadOutcome::Unresolved
                }
            }
        };
        (key, outcome)
    }))
    .buffer_unordered(HEAD_CONCURRENCY)
    .collect()
    .await
}

async fn cache_get(
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
    rule: &str,
    side: ParitySide,
    keys: &[String],
) -> HashMap<String, ParityCacheEntry> {
    let Some(mutex) = failures else {
        return HashMap::new();
    };
    let refs: Vec<&str> = keys.iter().map(|s| s.as_str()).collect();
    let db = mutex.lock().await;
    db.parity_cache_get_many(rule, side, &refs)
        .unwrap_or_default()
}

async fn cache_put(
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
    rule: &str,
    side: ParitySide,
    entries: &[(String, ParityCacheEntry)],
) {
    if entries.is_empty() {
        return;
    }
    let Some(mutex) = failures else {
        return;
    };
    let now = super::current_unix_seconds();
    let mut db = mutex.lock().await;
    if let Err(e) = db.parity_cache_put_many(rule, side, entries, now) {
        warn!("parity cache write failed for rule '{rule}': {e}");
    }
}

/// Async driver: list both sides, diff, build the outcome.
///
/// SOURCE is filtered through `should_replicate` so the audit covers
/// EXACTLY what replication acts on; each source key is rewritten into the
/// dest namespace. DEST is listed with the same marker/internal skip (but
/// not the source globs — an excluded-but-present dest object is a genuine
/// orphan). Caps total scanned at `MAX_PARITY_OBJECTS`; `truncated=true`
/// rather than hang for huge buckets.
/// Paginate one bucket+prefix, feeding each object to `keep`. `keep` inserts
/// (and returns `Ok(true)` if it consumed a slot, `Ok(false)` to skip). Caps
/// at `max` kept objects. Returns `Ok(truncated)`.
#[allow(clippy::too_many_arguments)]
async fn scan_prefix(
    engine: &DynEngine,
    bucket: &str,
    prefix: &str,
    max: usize,
    progress: Option<&ParityProgress<'_>>,
    // Objects scanned on the OTHER side already — so the reported running total
    // is cumulative across both side-scans, not reset per side.
    base_scanned: usize,
    mut keep: impl FnMut(&str, &FileMetadata) -> Result<bool, String>,
) -> Result<(bool, usize), String> {
    let mut kept = 0usize;
    let mut seen = 0usize;
    let mut page_idx = 0usize;
    let mut truncated = false;
    let mut pager = crate::job_loop::Pager::fresh();
    'pages: while pager.begin_page().is_some() {
        // Fast cancel check EVERY page — in-process AtomicBool, no lock.
        if let Some(p) = progress {
            if p.cancelled_local() {
                return Err(CANCELLED.to_string());
            }
            // Progress write is throttled (lock-bearing) — the count is a
            // spinner-grade estimate, not worth the global-mutex churn per page.
            if should_flush(page_idx as u64, PROGRESS_FLUSH_EVERY_N_PAGES as u64) {
                let db = p.db.lock().await;
                let now = super::current_unix_seconds();
                let _ = db.parity_result_progress(p.rule, (base_scanned + seen) as i64, now);
            }
        }
        page_idx += 1;
        // Retry transient list errors (Hetzner 503 throttle on a long scan)
        // with backoff instead of failing the whole audit on one blip.
        // LITE list (metadata=false) — no per-object HEAD; logical metadata for
        // delta/eligible keys is resolved afterwards (cache, then HEAD on a miss).
        let page = {
            let mut attempt = 0u32;
            loop {
                match engine
                    .list_objects(bucket, prefix, None, PAGE_SIZE, pager.token(), false)
                    .await
                {
                    Ok(p) => break p,
                    Err(e) => {
                        let msg = e.to_string();
                        attempt += 1;
                        if attempt >= LIST_MAX_ATTEMPTS
                            || !crate::transfer::is_transient_copy_error(&msg)
                        {
                            return Err(format!("list {bucket}/{prefix} page failed: {msg}"));
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64))
                            .await;
                    }
                }
            }
        };
        for (key, meta) in &page.objects {
            if kept >= max {
                truncated = true;
                break 'pages;
            }
            seen += 1;
            if keep(key, meta)? {
                kept += 1;
            }
        }
        if !pager.advance(page.is_truncated, page.next_continuation_token) {
            break;
        }
    }
    // Final report for this side so the dest scan starts from the right base.
    if let Some(p) = progress {
        let db = p.db.lock().await;
        let now = super::current_unix_seconds();
        let _ = db.parity_result_progress(p.rule, (base_scanned + seen) as i64, now);
    }
    Ok((truncated || pager.truncated_by_page_budget(), seen))
}

/// Evict cache rows for objects no longer present (deleted since last scan) —
/// bounds growth + drops stale rows. ONLY runs after a COMPLETE scan: a
/// truncated scan didn't see every key, so it can't tell deleted from unscanned.
/// Each side is pruned against ITS OWN live key set. Best-effort (logs on error).
async fn prune_parity_cache(
    rule: &ReplicationRule,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
    truncated: bool,
    source: &BTreeMap<String, ObjState>,
    dest: &BTreeMap<String, ObjState>,
) {
    if truncated {
        return;
    }
    let Some(mutex) = failures else { return };
    let src_live: Vec<String> = source.keys().cloned().collect();
    let dst_live: Vec<String> = dest.keys().cloned().collect();
    let mut db = mutex.lock().await;
    for (side, live) in [
        (ParitySide::Source, &src_live),
        (ParitySide::Dest, &dst_live),
    ] {
        if let Err(e) = db.parity_cache_retain(&rule.name, side, live) {
            warn!("parity cache prune failed for rule '{}': {e}", rule.name);
        }
    }
}

/// Audit a replication rule's parity: scan both sides (lite list), resolve
/// logical metadata for delta-eligible keys (cache-first, bounded HEAD burst),
/// diff, and annotate the sample findings. Phases in body:
///   1. scan SOURCE prefix → `source` map + `src_needs_logical`
///   2. scan DEST prefix → `dest` map + `dst_needs_logical` (+ provisional ownership)
///   3. prune the needs-logical sets to the keys where a HEAD changes the verdict
///   4. resolve logical metadata (HEAD burst) → unresolved count feeds `truncated`
///   5. prune stale cache, diff, annotate → `ParityOutcome`
pub async fn parity_audit(
    engine: &DynEngine,
    rule: &ReplicationRule,
    max_objects: usize,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
    progress: Option<ParityProgress<'_>>,
) -> Result<ParityOutcome, String> {
    let progress = progress.as_ref();
    let (inc, exc) = compile_rule_globs(rule).map_err(|e| e.to_string())?;
    let source_prefix = normalize_prefix(&rule.source.prefix);
    let dest_prefix = normalize_prefix(&rule.destination.prefix);

    let mut source: BTreeMap<String, ObjState> = BTreeMap::new();
    let mut dest: BTreeMap<String, ObjState> = BTreeMap::new();
    // Reverse map dest-key → raw source-key, so the failure-ledger join (keyed
    // by the worker's raw source_key) can be looked up from a dest-namespace
    // finding even when source.prefix != destination.prefix.
    let mut dest_to_source: HashMap<String, String> = HashMap::new();
    // Delta-eligible keys whose logical metadata wasn't in the lite list — these
    // need a HEAD (unless the parity cache already has them). Collected per side
    // as (storage_key, map_key) so we can write the resolved ObjState back.
    let mut src_needs_logical: Vec<String> = Vec::new();
    let mut dst_needs_logical: Vec<String> = Vec::new();

    // Each side gets its OWN budget (capped at max_objects) so a balanced large
    // mirror isn't spuriously truncated and a big source can't starve the dest
    // scan into emitting false 'missing' findings.
    //
    // LITE list (metadata=false) — no per-object HEAD. For delta objects the
    // lite list carries the DELTA-blob size/etag (not logical), so those keys
    // are queued for logical resolution (cache first, HEAD only on a miss).
    // On a SOURCE whose lite list is untrustworthy (encrypting/S3 → ciphertext
    // size/etag), EVERY key must be HEAD-resolved for a correct compare — the
    // symmetric guard to the dest side (finding #5 was one-directional).
    // ── Regime classification (cheap config read, no I/O) ──
    // PureMirror ⇒ dest is a verbatim byte-copy ⇒ compare STORED size+etag from
    // the lite list, ZERO HEADs. Transforming ⇒ re-derived bytes ⇒ logical HEAD.
    let src_lite_authoritative = engine.lite_list_carries_logical_facts(&rule.source.bucket);
    let dest_lite_authoritative = engine.lite_list_carries_logical_facts(&rule.destination.bucket);
    let reg = engine.bucket_policy_registry();
    let regime = classify_regime(
        src_lite_authoritative,
        dest_lite_authoritative,
        reg.compression_enabled(&rule.source.bucket),
        reg.compression_enabled(&rule.destination.bucket),
    );
    let pure_mirror = regime == Regime::PureMirror;

    // ── Phase 1: scan SOURCE prefix ──
    let (src_truncated, src_seen) = scan_prefix(
        engine,
        &rule.source.bucket,
        &source_prefix,
        max_objects,
        progress,
        0,
        |key, meta| {
            // dest_meta=None here → this is purely the glob/eligibility filter
            // ("would this key replicate at all"); the conflict policy is
            // irrelevant because every policy copies a missing destination.
            if !matches!(
                should_replicate(
                    key,
                    meta,
                    None,
                    ConflictPolicy::NewerWins,
                    false,
                    &inc,
                    &exc
                ),
                Decision::Copy { .. }
            ) {
                return Ok(false);
            }
            let dest_key = rewrite_key(&rule.source.prefix, &rule.destination.prefix, key)
                .map_err(|e| e.to_string())?;
            dest_to_source.insert(dest_key.clone(), key.to_string());
            // PureMirror skips logical resolution entirely (stored size+etag
            // from the lite list is the proof).
            if !pure_mirror
                && (!src_lite_authoritative || needs_logical_resolution(engine, key, meta))
            {
                src_needs_logical.push(key.to_string());
            }
            source.insert(dest_key, ObjState::from_metadata(meta));
            Ok(true)
        },
    )
    .await?;

    // On a backend whose lite list carries neither user_metadata (ownership)
    // nor plaintext size/etag (S3, or an actively-encrypting wrapper), EVERY
    // dest key must be HEAD-resolved: the lite entry can't be trusted for the
    // orphan-ownership check (#4) or the size/etag compare (#5).
    // ── Phase 2: scan DEST prefix ──
    let (dst_truncated, _dst_seen) = scan_prefix(
        engine,
        &rule.destination.bucket,
        &dest_prefix,
        max_objects,
        progress,
        src_seen,
        |key, meta| {
            if is_skippable_key(key) {
                return Ok(false);
            }
            let mut st = ObjState::from_metadata(meta);
            // Ownership from the lite entry is authoritative ONLY when the
            // backend surfaces user_metadata in lists; otherwise it is
            // provisional (default not-owned) and overlaid from the HEAD.
            st.owned_by_rule = if dest_lite_authoritative {
                Some(event_consumer::owned_by_rule(meta, &rule.name))
            } else {
                Some(false)
            };
            if !pure_mirror
                && (!dest_lite_authoritative || needs_logical_resolution(engine, key, meta))
            {
                dst_needs_logical.push(key.to_string());
            }
            dest.insert(key.to_string(), st);
            Ok(true)
        },
    )
    .await?;

    // Both sides listed → publish the denominator so the UI bar goes determinate.
    if let Some(p) = progress {
        p.set_total(source.len() as u64).await;
    }

    // Logical metadata (real size/etag via HEAD) only changes the verdict for a
    // key that exists on BOTH sides (match vs mismatch). A source key MISSING on
    // the destination is "missing" regardless of its logical size — HEADing it is
    // pure waste. So scope the source HEAD-burst to the intersection with the
    // dest key set. This is what makes a verify against an empty/sparse dest fast:
    // every source key is trivially missing, so zero source HEADs are issued.
    // ── Phase 3: prune needs-logical to keys where a HEAD changes the verdict ──
    src_needs_logical.retain(|raw| {
        match rewrite_key(&rule.source.prefix, &rule.destination.prefix, raw) {
            Ok(dk) => dest.contains_key(&dk),
            Err(_) => false,
        }
    });
    // Symmetric: a dest ORPHAN (not on source) is "extra on dest" regardless of
    // its logical SIZE, so it needs no size-HEAD. BUT when the lite list can't
    // carry ownership (S3/encrypting dest), an orphan is exactly the key whose
    // OWNERSHIP must be HEAD-resolved (delete-safety) — keep those.
    dst_needs_logical.retain(|dk| source.contains_key(dk) || !dest_lite_authoritative);

    // Resolve logical metadata for the delta-eligible keys: parity cache first
    // (HEAD-free — the win), then a bounded HEAD burst for the misses, writing
    // results back into the cache so the NEXT verify is HEAD-free too.
    // The HEAD-burst tail can be the slow part on a cold cache, so honour a
    // cancel at each resolve boundary (the per-page check above only covers
    // listing).
    // ── Phase 4: resolve logical metadata (cache-first, bounded HEAD burst) ──
    if let Some(p) = progress {
        p.check_cancel().await?;
    }
    let src_unresolved = resolve_logical(
        engine,
        rule,
        &rule.source.bucket,
        &rule.source.prefix,
        &rule.destination.prefix,
        &src_needs_logical,
        &mut source,
        failures,
    )
    .await;
    if let Some(p) = progress {
        p.check_cancel().await?;
    }
    let dst_unresolved = resolve_logical_dest(
        engine,
        rule,
        &rule.destination.bucket,
        &dst_needs_logical,
        &mut dest,
        failures,
    )
    .await;

    // A transient HEAD that left a key unresolved means the audit couldn't see
    // every object — treat it as truncated (partial) so it never reports a
    // false in_sync off keys that were silently dropped from the compare.
    let unresolved = src_unresolved + dst_unresolved;
    let truncated = src_truncated || dst_truncated || unresolved > 0;

    if truncated {
        warn!(
            "parity audit for rule '{}' is partial (scan cap {} objects, {} unresolved HEADs)",
            rule.name, max_objects, unresolved
        );
    }

    // Phase 5: prune stale cache rows, then diff + annotate into the outcome.
    prune_parity_cache(rule, failures, truncated, &source, &dest).await;

    let source_objects = source.len() as u64;
    let dest_objects = dest.len() as u64;
    let mut diff = diff_parity(&source, &dest, regime);

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
        regime,
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

    #[test]
    fn classify_regime_truth_table() {
        use Regime::*;
        // Both authoritative + same compression → pure.
        assert_eq!(classify_regime(true, true, true, true), PureMirror);
        assert_eq!(classify_regime(true, true, false, false), PureMirror);
        // Either side non-authoritative (S3 / encrypting) → transforming.
        assert_eq!(classify_regime(false, true, true, true), Transforming);
        assert_eq!(classify_regime(true, false, true, true), Transforming);
        // Compression differs → transforming (re-delta changes stored size).
        assert_eq!(classify_regime(true, true, true, false), Transforming);
    }

    #[test]
    fn compare_pair_stored_verbatim_and_redelta() {
        // Verbatim copy: equal stored size + etag → exact-copy match.
        let a = st_stored(1000, Some("etag-x"));
        assert_eq!(compare_pair_stored(&a, &a).0, FindingKind::Match);
        // Etag differs at equal stored size → mismatch.
        let b = st_stored(1000, Some("etag-y"));
        assert_eq!(compare_pair_stored(&a, &b).0, FindingKind::ChecksumMismatch);
        // Stored size differs (a re-delta) → flagged, unverifiable (needs HEAD).
        let c = st_stored(1234, Some("etag-x"));
        let (kind, _, unverifiable, _) = compare_pair_stored(&a, &c);
        assert_eq!(kind, FindingKind::ChecksumMismatch);
        assert!(
            unverifiable,
            "re-delta is flagged as needs-checksum, not asserted-wrong"
        );
    }

    fn st_stored(stored: u64, etag: Option<&str>) -> ObjState {
        ObjState {
            sha256: None,
            size: stored,
            stored_size: stored,
            etag: etag.map(|s| s.to_string()),
            multipart_parts: None,
            created_at: None,
            owned_by_rule: None,
        }
    }

    fn st(sha: Option<&str>, size: u64, etag: Option<&str>, parts: Option<u32>) -> ObjState {
        ObjState {
            sha256: sha.map(|s| s.to_string()),
            size,
            stored_size: size,
            etag: etag.map(|s| s.to_string()),
            multipart_parts: parts,
            created_at: None,
            owned_by_rule: None,
        }
    }

    // ─────────────── cache freshness guard (false-"in-sync" defence) ───────────

    fn entry(stored: Option<&str>) -> ParityCacheEntry {
        ParityCacheEntry {
            sha256: Some("logical".into()),
            size: 1,
            etag: Some("logical-etag".into()),
            stored_etag: stored.map(str::to_string),
        }
    }

    // ─────────────── progress flush cadence (pure decision) ────────────────────

    #[test]
    fn should_flush_at_every_th_page_boundary() {
        // Flush at page 0 and every `every`-th page; nowhere else.
        assert!(should_flush(0, 64), "page 0 always flushes (first page)");
        assert!(should_flush(64, 64));
        assert!(should_flush(128, 64));
        assert!(!should_flush(63, 64));
        assert!(!should_flush(65, 64));
        assert!(!should_flush(127, 64));
        // `every` of 1 flushes every page (the degenerate fine-grained case).
        assert!(should_flush(0, 1) && should_flush(1, 1) && should_flush(2, 1));
        // The configured cadence: flush every 8 pages so the live count ticks.
        let n = PROGRESS_FLUSH_EVERY_N_PAGES as u64;
        assert!(should_flush(0, n) && should_flush(8, n) && !should_flush(7, n));
    }

    #[test]
    fn should_flush_never_when_every_is_zero() {
        // Defensive: a zero cadence must never flush (avoid panic on `% 0`),
        // so a misconfigured constant degrades to no mid-scan writes rather
        // than panicking — the settle flush still fires at scan end.
        assert!(!should_flush(0, 0));
        assert!(!should_flush(64, 0));
    }

    #[test]
    fn cache_hit_only_when_stored_etag_unchanged() {
        // Same stored blob etag → trust the cache (the warm-path win).
        assert!(cache_hit_fresh(
            &entry(Some("blob-v1")),
            &Some("blob-v1".into())
        ));
        // Overwritten in place → stored etag changed → MISS → re-HEAD.
        // This is the false-"in-sync" defence: a changed object is never trusted.
        assert!(!cache_hit_fresh(
            &entry(Some("blob-v1")),
            &Some("blob-v2".into())
        ));
    }

    #[test]
    fn cache_miss_when_either_etag_absent() {
        // No etag either side → can't prove unchanged → MISS (re-read, don't risk
        // a stale verdict).
        assert!(!cache_hit_fresh(&entry(None), &None));
        assert!(!cache_hit_fresh(&entry(None), &Some("x".into())));
        assert!(!cache_hit_fresh(&entry(Some("x")), &None));
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

    #[tokio::test]
    async fn check_cancel_honours_in_process_flag_without_a_db_hit() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let db = tokio::sync::Mutex::new(crate::config_db::ConfigDb::in_memory("t").unwrap());
        let flag = AtomicBool::new(true);
        let p = ParityProgress {
            db: &db,
            rule: "r",
            cancel: &flag,
        };
        assert!(p.cancelled_local());
        assert_eq!(p.check_cancel().await.unwrap_err(), CANCELLED);
        // Cleared flag → local check is false (DB branch would run; here the row
        // is absent so it returns Ok).
        flag.store(false, Ordering::Relaxed);
        assert!(!p.cancelled_local());
        assert!(p.check_cancel().await.is_ok());
    }

    #[test]
    fn diff_all_match() {
        let s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let d = s.clone();
        let r = diff_parity(&s, &d, Regime::Transforming);
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
        let r = diff_parity(&s, &d, Regime::Transforming);
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
        let r = diff_parity(&s, &d, Regime::Transforming);
        assert_eq!(r.matched, 1);
        assert_eq!(r.orphan_on_dest, 1);
        assert_eq!(r.orphan_samples[0].key, "z");
    }

    #[test]
    fn diff_one_mismatch() {
        let s = map(&[("a", st(Some("h"), 1, None, None))]);
        let d = map(&[("a", st(Some("DIFFERENT"), 1, None, None))]);
        let r = diff_parity(&s, &d, Regime::Transforming);
        assert_eq!(r.checksum_mismatch, 1);
        assert_eq!(r.matched, 0);
        assert_eq!(r.mismatch_samples[0].key, "a");
    }

    #[test]
    fn diff_empty_empty() {
        let r = diff_parity(&BTreeMap::new(), &BTreeMap::new(), Regime::Transforming);
        assert_eq!(r, ParityDiff::default());
    }

    #[test]
    fn diff_source_empty_all_orphan() {
        let d = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let r = diff_parity(&BTreeMap::new(), &d, Regime::Transforming);
        assert_eq!(r.orphan_on_dest, 2);
        assert_eq!(r.missing_on_dest, 0);
    }

    #[test]
    fn diff_dest_empty_all_missing() {
        let s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let r = diff_parity(&s, &BTreeMap::new(), Regime::Transforming);
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
        let r = diff_parity(&s, &d, Regime::Transforming);
        assert_eq!(r.matched, 2);
        assert_eq!(r.unverifiable, 1);
    }

    #[test]
    fn diff_sample_caps_at_100() {
        let mut s: BTreeMap<String, ObjState> = BTreeMap::new();
        for i in 0..250 {
            s.insert(format!("k{i:04}"), st(Some("h"), 1, None, None));
        }
        let r = diff_parity(&s, &BTreeMap::new(), Regime::Transforming);
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
                stored_size: size,
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
            let r = diff_parity(&s, &d, Regime::Transforming);
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
            let r = diff_parity(&s, &d, Regime::Transforming);
            prop_assert!(r.missing_samples.len() <= SAMPLE_CAP);
            prop_assert!(r.orphan_samples.len() <= SAMPLE_CAP);
            prop_assert!(r.mismatch_samples.len() <= SAMPLE_CAP);
        }

        #[test]
        fn in_sync_iff_all_zero_and_not_truncated(
            s in arb_map(), d in arb_map(), truncated in any::<bool>()
        ) {
            let r = diff_parity(&s, &d, Regime::Transforming);
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
        let mut diff = diff_parity(&source, &dest, Regime::Transforming);
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
        let mut diff = diff_parity(&source, &dest, Regime::Transforming);
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
        let mut diff = diff_parity(&source, &dest, Regime::Transforming);
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
        let mut diff = diff_parity(&source, &dest, Regime::Transforming);
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
