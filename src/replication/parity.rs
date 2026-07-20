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

/// Minimum wall-clock interval between progress flushes. Combined with the
/// page-count throttle above, this ensures a fast scan (sub-second pages)
/// still publishes intermediate counts: flush when EITHER N pages OR this
/// interval has elapsed since the last flush. Caps at ≤4 writes/s to limit
/// global-mutex contention with the IAM/admin path.
const PROGRESS_FLUSH_MIN_INTERVAL_MS: u64 = 250;

/// Pure decision: should the per-page progress counter flush to the DB at this
/// page index? Flushes on every `every`-th page (page 0, `every`, `2*every`, …).
/// Extracted so the cadence is unit-testable without a DB / scan.
fn should_flush(page: u64, every: u64) -> bool {
    every > 0 && page.is_multiple_of(every)
}

/// Pure decision: should we flush progress now, considering both the page-count
/// throttle and wall-clock elapsed since the last flush? Flushes when EITHER
/// the page-count condition fires OR enough time has passed. `elapsed_ms` is
/// milliseconds since the last flush (or `u64::MAX` if no flush has happened
/// yet, which guarantees the first page always flushes).
fn should_flush_now(page: u64, every: u64, elapsed_ms: u64, min_interval_ms: u64) -> bool {
    should_flush(page, every) || elapsed_ms >= min_interval_ms
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

    /// Publish the denominator once it's known (best-effort; a failed write
    /// just leaves the bar indeterminate).
    async fn set_total(&self, total: u64) {
        let now = super::current_unix_seconds();
        let db = self.db.lock().await;
        let _ = db.parity_result_set_total(self.rule, total as i64, now);
    }

    /// Best-effort write of the running objects-examined counter.
    async fn flush_scanned(&self, scanned: u64) {
        let db = self.db.lock().await;
        let now = super::current_unix_seconds();
        let _ = db.parity_result_progress(self.rule, scanned as i64, now);
    }
}

/// Which comparison regime a rule runs in.
///
/// `PureMirror`: both sides' lite list carries LOGICAL facts (filesystem xattr —
/// sha256/size/etag inline), so the same `compare_pair` runs on lite data with
/// ZERO HEADs. `Transforming`: at least one side's lite list is untrustworthy
/// (S3 / encrypting → ciphertext size/etag), so logical facts are HEAD-resolved
/// first. The COMPARE is identical either way — only the resolution path differs.
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

/// PURE classifier: `PureMirror` iff BOTH sides carry trustworthy lite facts.
/// The compare is logical, so compression settings don't affect its validity.
pub fn classify_regime(src_lite_authoritative: bool, dst_lite_authoritative: bool) -> Regime {
    if src_lite_authoritative && dst_lite_authoritative {
        Regime::PureMirror
    } else {
        Regime::Transforming
    }
}

/// Per-category sample cap surfaced to the UI (exact counts stay unbounded).
pub const SAMPLE_CAP: usize = 100;
/// Default ceiling on total objects scanned across both sides before we stop
/// and report `truncated=true` (2× usage_scanner's 100k — two prefixes).
/// Override with `DGP_PARITY_MAX_OBJECTS` for buckets larger than ~100k objects.
pub const MAX_PARITY_OBJECTS: usize = 200_000;

/// The effective parity scan cap: `DGP_PARITY_MAX_OBJECTS` env override, else
/// [`MAX_PARITY_OBJECTS`]. Clamped to a sane floor so it can't be set to 0.
pub fn max_parity_objects() -> usize {
    crate::config::env_parse_with_default("DGP_PARITY_MAX_OBJECTS", MAX_PARITY_OBJECTS).max(1000)
}
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
    /// STORED-blob content-version token for the parity cache (NOT the logical
    /// etag). For a DELTA object this is `"{delta_size}:{ref_sha256}"` — it
    /// changes whenever the delta is re-encoded (e.g. against a rotated
    /// reference), which the logical `etag`/md5 does NOT (same logical file →
    /// same md5). For passthrough it IS the logical etag/md5 (stored verbatim).
    /// This is what busts a stale parity-cache hit after an overwrite; using the
    /// logical etag here let a re-delta collide and serve a stale size.
    pub stored_version: Option<String>,
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
        // Stored-blob content-version for the parity cache.
        //  - DELTA with a resolved ref_sha256 (filesystem HEAD): the
        //    (delta_size, ref_sha256) pair — changes on any re-encode, unlike
        //    the logical md5 (stable across re-deltas → the stale-cache bug).
        //  - DELTA stub from an S3 lite list (ref_sha256 empty): fold delta_size
        //    together with the S3 object ETag (which the lite list already put in
        //    `md5`) so we never DROP the strongest S3 content-version.
        //  - Passthrough: the logical etag (stored verbatim).
        let stored_version = match &m.storage_info {
            crate::types::StorageInfo::Delta {
                delta_size,
                ref_sha256,
                ..
            } if !ref_sha256.is_empty() => Some(format!("d:{delta_size}:{ref_sha256}")),
            crate::types::StorageInfo::Delta { delta_size, .. } => {
                Some(format!("d:{delta_size}:{}", etag.as_deref().unwrap_or("")))
            }
            _ => etag.clone(),
        };
        ObjState {
            sha256,
            size: m.file_size,
            etag,
            multipart_parts,
            created_at: Some(m.created_at.timestamp_millis()),
            owned_by_rule: None,
            stored_version,
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
    // unverifiable (no comparable checksum on both sides, so we could only prove
    // the sizes match — not the bytes).
    (
        FindingKind::Match,
        Some(Verifier::SizeOnly),
        true,
        "size matches; no comparable checksum on both sides to prove byte-equality".to_string(),
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
    /// How logical facts were sourced: `pure_mirror` (from the listing — zero
    /// HEADs) vs `transforming` (cache-first HEAD resolution). Same compare
    /// either way; drives the provenance footnote. Defaults to `transforming`
    /// for outcomes serialized before this field existed.
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
    /// The ONE authoritative verdict, derived server-side (see `derive_verdict`)
    /// so the frontend renders a single conclusion instead of re-deriving tone
    /// from the raw counts. Defaults to `Safe` for outcomes serialized before
    /// this field existed (harmless — a stale row is re-verified on view).
    #[serde(default = "default_verdict")]
    pub verdict: Verdict,
    /// One-sentence, human plain-language summary matching `verdict` — the
    /// headline the UI shows verbatim. Empty for pre-field outcomes.
    #[serde(default)]
    pub verdict_summary: String,
}

/// The single authoritative conclusion of a parity audit. Replaces the
/// frontend's `toneFor` count-arithmetic (VerifyTab.tsx) so "685 differences"
/// and "no differences while not in_sync" can never disagree with the halo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every source object is present on the destination and proven equal.
    #[default]
    Safe,
    /// Nothing is missing/extra/different, but the scan couldn't PROVE full
    /// equality — capped scan, unresolved HEADs, or size-only matches. Benign
    /// but not a clean bill of health.
    Incomplete,
    /// Real divergence: something is missing, extra, or its bytes differ.
    AtRisk,
}

fn default_verdict() -> Verdict {
    Verdict::Safe
}

/// PURE: the single conclusion + its plain-language sentence, from the counts.
/// Precedence: any real divergence (missing/extra/mismatch) → AtRisk; else a
/// truncated or size-only-only or otherwise-not-in_sync scan → Incomplete;
/// else Safe. `matched`/`source_objects` fill the sentence's denominator.
pub fn derive_verdict(
    in_sync: bool,
    truncated: bool,
    missing_on_dest: u64,
    orphan_on_dest: u64,
    checksum_mismatch: u64,
    unverifiable: u64,
    matched: u64,
) -> (Verdict, String) {
    let real_diffs = missing_on_dest + orphan_on_dest + checksum_mismatch;
    if real_diffs > 0 {
        let mut parts = Vec::new();
        if missing_on_dest > 0 {
            parts.push(format!("{missing_on_dest} missing on destination"));
        }
        if orphan_on_dest > 0 {
            parts.push(format!("{orphan_on_dest} extra on destination"));
        }
        if checksum_mismatch > 0 {
            parts.push(format!("{checksum_mismatch} with differing bytes"));
        }
        return (
            Verdict::AtRisk,
            format!("The copy has drifted: {}.", parts.join(", ")),
        );
    }
    if in_sync {
        return (
            Verdict::Safe,
            format!("All {matched} objects are present and verified identical."),
        );
    }
    // Not in_sync with no real diffs → couldn't fully prove equality.
    let why = if truncated {
        "the scan was capped or some objects couldn't be read, so completeness isn't proven"
    } else if unverifiable > 0 {
        "some objects could only be matched by size, not by checksum"
    } else {
        "equality couldn't be fully proven"
    };
    (
        Verdict::Incomplete,
        format!("Nothing is missing or different, but {why}."),
    )
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

/// True for a small VERBATIM sidecar (`.sha1`/`.sha256`/`.sha512`) that is
/// stored byte-for-byte on every backend — its lite size/etag are authoritative
/// even on S3, so it NEVER needs a logical HEAD. These make up a large fraction
/// of the object count (each artifact ships a checksum sidecar), so skipping
/// their HEADs roughly halves a HEAD-bound S3 parity sweep. Correctness: a
/// sidecar can still be a mismatch (size/etag differ) or an orphan — but those
/// are decided from the lite entry, which is exact for a verbatim object.
fn is_verbatim_sidecar(engine: &DynEngine, key: &str, meta: &FileMetadata) -> bool {
    !meta.is_delta()
        && !engine.is_delta_eligible_key(key)
        && (key.ends_with(".sha1") || key.ends_with(".sha256") || key.ends_with(".sha512"))
}

/// Overlay logical (sha256, size, etag) onto the `ObjState` in `map` for `key`.
fn apply_logical(map: &mut BTreeMap<String, ObjState>, map_key: &str, e: &ParityCacheEntry) {
    if let Some(st) = map.get_mut(map_key) {
        st.sha256 = e.sha256.clone();
        st.size = e.size;
        st.etag = e.etag.clone();
        // Overlay the TRUE created_at from the resolving HEAD, replacing the lite
        // list's value (S3 last_modified) so remediation's newer-wins conflict
        // resolution compares real creation times (H49). Keep the existing value
        // if the HEAD didn't carry one.
        if e.created_at.is_some() {
            st.created_at = e.created_at;
        }
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
        created_at: Some(m.created_at.timestamp_millis()),
    }
}

/// The STORED-blob content-version the lite list recorded for `map_key`. Read
/// from the ObjState BEFORE any logical overlay. This is `stored_version` (the
/// delta-aware token), NOT the logical `etag` — using the logical etag let a
/// re-delta against a rotated reference collide and serve a stale cached size.
fn lite_stored_version(map: &BTreeMap<String, ObjState>, map_key: &str) -> Option<String> {
    map.get(map_key).and_then(|st| st.stored_version.clone())
}

/// A cache hit is only valid when the stored blob hasn't changed since it was
/// cached: the cached `stored_etag` must equal the current lite `stored_etag`.
/// A `None`/`None` pair (no etag either side) is treated as a MISS — we can't
/// prove the object is unchanged, so we re-read rather than risk a stale verdict.
fn cache_hit_fresh(cached: &ParityCacheEntry, lite_stored_version: &Option<String>) -> bool {
    matches!((&cached.stored_etag, lite_stored_version), (Some(a), Some(b)) if a == b)
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
    progress: Option<&ParityProgress<'_>>,
    progress_base: u64,
) -> Vec<String> {
    if raw_keys.is_empty() {
        return Vec::new();
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
        let lite = lite_stored_version(source, &dk);
        match cached.get(&dk) {
            Some(e) if cache_hit_fresh(e, &lite) => apply_logical(source, &dk, e),
            _ => miss_raw.push(raw),
        }
    }
    // Cache hits resolve instantly — count them before the burst starts.
    let hits = (raw_keys.len() - miss_raw.len()) as u64;
    if let Some(p) = progress {
        p.flush_scanned(progress_base + hits).await;
    }
    let fresh = head_burst(engine, bucket, &miss_raw, progress, progress_base + hits).await;
    let mut to_cache: Vec<(String, ParityCacheEntry)> = Vec::new();
    let mut unresolved_keys: Vec<String> = Vec::new();
    for (raw, outcome) in fresh {
        let Ok(dk) = rewrite_key(src_prefix, dst_prefix, &raw) else {
            continue;
        };
        match outcome {
            HeadOutcome::Resolved(meta) => {
                let stored = lite_stored_version(source, &dk);
                let e = cache_entry_from_meta(&meta, stored);
                apply_logical(source, &dk, &e);
                to_cache.push((dk, e));
            }
            // Raced delete → drop from the compare (it's genuinely gone).
            HeadOutcome::Gone => {
                source.remove(&dk);
            }
            // Transient HEAD failure → drop from the compare (a false verdict on
            // the untrusted lite size is worse, #16) and RETURN the key so the
            // driver also drops it from the OTHER side — else a one-sided drop
            // becomes a false missing/extra finding (#H5). The count still
            // marks the audit partial (#3).
            HeadOutcome::Unresolved => {
                source.remove(&dk);
                unresolved_keys.push(dk);
            }
        }
    }
    cache_put(failures, &rule.name, ParitySide::Source, &to_cache).await;
    unresolved_keys
}

/// Dest-side logical resolution: dest is keyed by its own raw key (== cache key).
/// Returns the count of keys left UNRESOLVED by a transient HEAD (see
/// `resolve_logical`). A non-zero count makes the audit partial.
#[allow(clippy::too_many_arguments)]
async fn resolve_logical_dest(
    engine: &DynEngine,
    rule: &ReplicationRule,
    bucket: &str,
    raw_keys: &[String],
    dest: &mut BTreeMap<String, ObjState>,
    failures: Option<&tokio::sync::Mutex<ConfigDb>>,
    progress: Option<&ParityProgress<'_>>,
    progress_base: u64,
) -> Vec<String> {
    if raw_keys.is_empty() {
        return Vec::new();
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
        let lite = lite_stored_version(dest, k);
        match cached.get(k) {
            Some(e) if cache_hit_fresh(e, &lite) => apply_logical(dest, k, e),
            _ => miss.push(k),
        }
    }
    // Cache hits resolve instantly — count them before the burst starts.
    let hits = (raw_keys.len() - miss.len()) as u64;
    if let Some(p) = progress {
        p.flush_scanned(progress_base + hits).await;
    }
    let fresh = head_burst(engine, bucket, &miss, progress, progress_base + hits).await;
    let mut to_cache: Vec<(String, ParityCacheEntry)> = Vec::new();
    let mut unresolved_keys: Vec<String> = Vec::new();
    for (k, outcome) in fresh {
        match outcome {
            HeadOutcome::Resolved(meta) => {
                if !lite_authoritative {
                    if let Some(st) = dest.get_mut(&k) {
                        st.owned_by_rule = Some(event_consumer::owned_by_rule(&meta, &rule.name));
                    }
                }
                let stored = lite_stored_version(dest, &k);
                let e = cache_entry_from_meta(&meta, stored);
                apply_logical(dest, &k, &e);
                to_cache.push((k, e));
            }
            HeadOutcome::Gone => {
                dest.remove(&k);
            }
            // Transient HEAD failure → drop and RETURN so the driver also drops
            // it from the source side (#H5: a one-sided drop is a false
            // orphan/missing). Count still marks the audit partial (#3).
            HeadOutcome::Unresolved => {
                dest.remove(&k);
                unresolved_keys.push(k);
            }
        }
    }
    cache_put(failures, &rule.name, ParitySide::Dest, &to_cache).await;
    unresolved_keys
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
/// Flushes running progress (`base` + completed) so the count keeps moving
/// through a long burst instead of freezing at the end-of-listing value.
/// Throttle-abort decision: trip only once the sliding window is FULL and its
/// transient count meets the threshold. Window-not-full never trips (so a short
/// startup burst or a small key set can't abort). Pure for unit testing.
fn throttle_window_tripped(
    window_len: usize,
    transient_count: usize,
    window_cap: usize,
    min_transient: usize,
) -> bool {
    window_len >= window_cap && transient_count >= min_transient
}

async fn head_burst(
    engine: &DynEngine,
    bucket: &str,
    keys: &[&String],
    progress: Option<&ParityProgress<'_>>,
    base: u64,
) -> Vec<(String, HeadOutcome)> {
    use futures::stream::StreamExt;
    // FLAT pipeline (was batched-with-barrier): keep N HEADs continuously
    // in flight instead of firing N then waiting for ALL N — the barrier idled
    // the fast HEADs behind the slowest one in every batch, the dominant cost of
    // a slow S3 sweep. Peak concurrency is unchanged (still bounded by N), so the
    // throttle posture that motivated the 50→10 drop (Hetzner Ceph 503 SlowDown,
    // 2026-07-09) is preserved. Two independent backstops remain: the SDK's
    // adaptive rate limiter paces ALL requests after one 503, and the
    // consecutive-transient circuit breaker below aborts a throttled sweep.
    // Default 15 (modestly above 10); env-tunable for cautious prod dialing.
    let concurrency: usize =
        crate::config::env_parse_with_default("DGP_PARITY_HEAD_CONCURRENCY", 15usize).clamp(1, 64);
    // Abort on a SUSTAINED transient failure RATIO over a sliding window, not a
    // consecutive run: under `buffer_unordered` outcomes arrive out of order, so
    // an occasional lucky Resolved would reset a consecutive counter and let a
    // throttled backend grind on. A windowed ratio (≥ABORT_RATIO transient in the
    // last WINDOW outcomes, once the window is full) catches sustained partial
    // throttling the consecutive signal misses. The SDK adaptive limiter still
    // paces every request after one 503; this is the belt over that suspenders.
    const ABORT_WINDOW: usize = 40;
    const ABORT_TRANSIENT_MIN: usize = 32; // ≥80% of the window transient → abort
    const FLUSH_EVERY_N_HEADS: usize = 500;
    let owned: Vec<String> = keys.iter().map(|k| (*k).clone()).collect();
    let mut out: Vec<(String, HeadOutcome)> = Vec::with_capacity(owned.len());

    let mut stream = futures::stream::iter(owned.iter().cloned().map(|key| async move {
        let outcome = match engine.head(bucket, &key).await {
            Ok(m) => HeadOutcome::Resolved(Box::new(m)),
            Err(e) => {
                let s = e.to_string().to_ascii_lowercase();
                if s.contains("not found") || s.contains("nosuchkey") {
                    HeadOutcome::Gone
                } else {
                    HeadOutcome::Unresolved
                }
            }
        };
        (key, outcome)
    }))
    .buffer_unordered(concurrency);

    let mut window: std::collections::VecDeque<bool> = std::collections::VecDeque::new();
    let mut window_transient = 0usize;
    let mut aborted = false;
    while let Some((key, outcome)) = stream.next().await {
        let is_transient = matches!(outcome, HeadOutcome::Unresolved);
        window.push_back(is_transient);
        if is_transient {
            window_transient += 1;
        }
        if window.len() > ABORT_WINDOW && window.pop_front() == Some(true) {
            window_transient -= 1;
        }
        out.push((key, outcome));
        // Only judge once the window is full, so a short burst at the very start
        // can't trip it (and a small key set never trips at all).
        if throttle_window_tripped(
            window.len(),
            window_transient,
            ABORT_WINDOW,
            ABORT_TRANSIENT_MIN,
        ) {
            aborted = true;
            break;
        }
        if let Some(p) = progress {
            if out.len().is_multiple_of(FLUSH_EVERY_N_HEADS) {
                p.flush_scanned(base + out.len() as u64).await;
            }
        }
    }
    // Dropping `stream` cancels the in-flight HEADs. Mark every not-yet-resolved
    // key Unresolved so the audit reports honest partial coverage (a throttled
    // backend is never counted as a mismatch) rather than grinding on.
    if aborted {
        drop(stream);
        let done: std::collections::HashSet<String> = out.iter().map(|(k, _)| k.clone()).collect();
        let leftover = owned.len() - out.len();
        tracing::warn!(
            "parity resolve on {bucket}: ≥{ABORT_TRANSIENT_MIN}/{ABORT_WINDOW} recent HEADs failed \
             transiently — marking the remaining {leftover} keys unresolved (partial audit) \
             instead of grinding a throttled backend"
        );
        for k in &owned {
            if !done.contains(k) {
                out.push((k.clone(), HeadOutcome::Unresolved));
            }
        }
    }
    out
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
    let mut last_flush: Option<std::time::Instant> = None;
    let mut pager = crate::job_loop::Pager::fresh();
    'pages: while pager.begin_page().is_some() {
        // Fast cancel check EVERY page — in-process AtomicBool, no lock.
        if let Some(p) = progress {
            if p.cancelled_local() {
                return Err(CANCELLED.to_string());
            }
            // Progress write is throttled (lock-bearing) — flush when EITHER
            // N pages OR 250ms have elapsed since the last flush, whichever
            // comes first. On a fast scan (sub-second pages) the time arm
            // fires; on a slow scan the page arm fires. Caps at ≤4 writes/s.
            let elapsed_ms = last_flush
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(u64::MAX);
            if should_flush_now(
                page_idx as u64,
                PROGRESS_FLUSH_EVERY_N_PAGES as u64,
                elapsed_ms,
                PROGRESS_FLUSH_MIN_INTERVAL_MS,
            ) {
                p.flush_scanned((base_scanned + seen) as u64).await;
                last_flush = Some(std::time::Instant::now());
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
        p.flush_scanned((base_scanned + seen) as u64).await;
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
    // PureMirror ⇒ both lite lists carry logical facts ⇒ compare directly,
    // ZERO HEADs. Transforming ⇒ logical facts need cache/HEAD resolution.
    let src_lite_authoritative = engine.lite_list_carries_logical_facts(&rule.source.bucket);
    let dest_lite_authoritative = engine.lite_list_carries_logical_facts(&rule.destination.bucket);
    let regime = classify_regime(src_lite_authoritative, dest_lite_authoritative);
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
            // PureMirror skips resolution: the lite entry already carries the
            // logical sha/size/etag (filesystem xattr), so the compare is direct.
            // A verbatim sidecar (.sha1/.sha256/.sha512) is exact from the lite
            // list on ANY backend — skip its HEAD even when the list isn't
            // otherwise authoritative (the S3 sweep's biggest avoidable cost).
            if !pure_mirror
                && !is_verbatim_sidecar(engine, key, meta)
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
    let (dst_truncated, dst_seen) = scan_prefix(
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
            // Skip the HEAD for a verbatim sidecar: its size/etag are exact from
            // the lite list, and ownership no longer gates deletes (faithful
            // mirror deletes any source-absent key regardless of provenance), so
            // the ownership overlay a HEAD would provide changes no verdict.
            if !pure_mirror
                && !is_verbatim_sidecar(engine, key, meta)
                && (!dest_lite_authoritative || needs_logical_resolution(engine, key, meta))
            {
                dst_needs_logical.push(key.to_string());
            }
            dest.insert(key.to_string(), st);
            Ok(true)
        },
    )
    .await?;

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

    // Denominator = everything the audit will examine, in the SAME units as
    // progress_scanned (listed objects + keys needing logical resolution) — so
    // the bar goes determinate here and climbs monotonically to exactly 100%.
    let listed = (src_seen + dst_seen) as u64;
    if let Some(p) = progress {
        p.set_total(listed + (src_needs_logical.len() + dst_needs_logical.len()) as u64)
            .await;
    }

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
    let src_unresolved_keys = resolve_logical(
        engine,
        rule,
        &rule.source.bucket,
        &rule.source.prefix,
        &rule.destination.prefix,
        &src_needs_logical,
        &mut source,
        failures,
        progress,
        listed,
    )
    .await;
    if let Some(p) = progress {
        p.check_cancel().await?;
    }
    let dst_unresolved_keys = resolve_logical_dest(
        engine,
        rule,
        &rule.destination.bucket,
        &dst_needs_logical,
        &mut dest,
        failures,
        progress,
        listed + src_needs_logical.len() as u64,
    )
    .await;

    // A key left unresolved on EITHER side must be dropped from BOTH maps
    // before the diff, or the merge-walk reports it as a false missing/orphan
    // (it exists on both, one side just couldn't HEAD it). #H5.
    for k in src_unresolved_keys.iter().chain(dst_unresolved_keys.iter()) {
        source.remove(k);
        dest.remove(k);
    }
    let src_unresolved = src_unresolved_keys.len();
    let dst_unresolved = dst_unresolved_keys.len();

    // Everything examined — land the counter exactly on the denominator.
    if let Some(p) = progress {
        p.flush_scanned(listed + (src_needs_logical.len() + dst_needs_logical.len()) as u64)
            .await;
    }

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

    let (verdict, verdict_summary) = derive_verdict(
        in_sync,
        truncated,
        diff.missing_on_dest,
        diff.orphan_on_dest,
        diff.checksum_mismatch,
        diff.unverifiable,
        diff.matched,
    );

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
        verdict,
        verdict_summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn classify_regime_truth_table() {
        use Regime::*;
        // Both sides' lite list carries logical facts → pure (zero HEADs).
        assert_eq!(classify_regime(true, true), PureMirror);
        // Either side non-authoritative (S3 / encrypting) → transforming.
        assert_eq!(classify_regime(false, true), Transforming);
        assert_eq!(classify_regime(true, false), Transforming);
        assert_eq!(classify_regime(false, false), Transforming);
    }

    #[test]
    fn derive_verdict_truth_table() {
        // Clean in-sync → Safe.
        let (v, s) = derive_verdict(true, false, 0, 0, 0, 0, 42);
        assert_eq!(v, Verdict::Safe);
        assert!(s.contains("42") && s.contains("identical"), "{s}");

        // Any real divergence → AtRisk, regardless of in_sync flag shape.
        assert_eq!(
            derive_verdict(false, false, 3, 0, 0, 0, 10).0,
            Verdict::AtRisk
        );
        assert_eq!(
            derive_verdict(false, false, 0, 2, 0, 0, 10).0,
            Verdict::AtRisk
        );
        assert_eq!(
            derive_verdict(false, false, 0, 0, 1, 0, 10).0,
            Verdict::AtRisk
        );
        // Mismatch wins even if something is also missing (both named in summary).
        let (v, s) = derive_verdict(false, false, 1, 0, 1, 0, 8);
        assert_eq!(v, Verdict::AtRisk);
        assert!(s.contains("missing") && s.contains("differing"), "{s}");

        // Not in_sync, no real diffs → Incomplete (never Safe, never AtRisk).
        // Truncated variant.
        let (v, s) = derive_verdict(false, true, 0, 0, 0, 0, 100);
        assert_eq!(v, Verdict::Incomplete);
        assert!(
            s.contains("capped") || s.contains("couldn't be read"),
            "{s}"
        );
        // Size-only variant.
        let (v, s) = derive_verdict(false, false, 0, 0, 0, 5, 100);
        assert_eq!(v, Verdict::Incomplete);
        assert!(s.contains("size"), "{s}");

        // AtRisk takes precedence over truncation.
        assert_eq!(
            derive_verdict(false, true, 1, 0, 0, 0, 9).0,
            Verdict::AtRisk
        );
    }

    fn st(sha: Option<&str>, size: u64, etag: Option<&str>, parts: Option<u32>) -> ObjState {
        ObjState {
            sha256: sha.map(|s| s.to_string()),
            size,
            etag: etag.map(|s| s.to_string()),
            multipart_parts: parts,
            created_at: None,
            owned_by_rule: None,
            // Passthrough default: stored version == logical etag. Delta-specific
            // cache tests set this explicitly via `..st(...)`.
            stored_version: etag.map(|s| s.to_string()),
        }
    }

    #[test]
    fn apply_logical_overlays_true_created_at() {
        // H49: the HEAD's real created_at must overlay the lite list's value
        // (S3 last_modified) so remediation's newer-wins compares correct times.
        let mut map = BTreeMap::new();
        map.insert(
            "k".to_string(),
            ObjState {
                created_at: Some(1000), // lite value (e.g. S3 last_modified)
                ..st(Some("old"), 1, None, None)
            },
        );
        let e = ParityCacheEntry {
            sha256: Some("new".into()),
            size: 2,
            etag: None,
            stored_etag: None,
            created_at: Some(5000), // true creation time from HEAD
        };
        apply_logical(&mut map, "k", &e);
        assert_eq!(map["k"].created_at, Some(5000), "true created_at overlaid");
        // A HEAD without created_at keeps the existing value.
        let e2 = ParityCacheEntry {
            created_at: None,
            ..e
        };
        apply_logical(&mut map, "k", &e2);
        assert_eq!(map["k"].created_at, Some(5000), "kept when HEAD has none");
    }

    // ─────────────── cache freshness guard (false-"in-sync" defence) ───────────

    fn entry(stored: Option<&str>) -> ParityCacheEntry {
        ParityCacheEntry {
            sha256: Some("logical".into()),
            size: 1,
            etag: Some("logical-etag".into()),
            stored_etag: stored.map(str::to_string),
            created_at: None,
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
    fn should_flush_now_fires_on_either_arm() {
        let n = PROGRESS_FLUSH_EVERY_N_PAGES as u64;
        let min = PROGRESS_FLUSH_MIN_INTERVAL_MS;

        // Page arm alone (not enough time): page 0 always flushes.
        assert!(should_flush_now(0, n, 0, min));
        assert!(should_flush_now(n, n, 10, min));

        // Time arm alone (not a page boundary, but enough time elapsed).
        assert!(should_flush_now(1, n, min, min));
        assert!(should_flush_now(3, n, min + 100, min));

        // Neither arm fires → no flush.
        assert!(!should_flush_now(1, n, 0, min));
        assert!(!should_flush_now(7, n, min - 1, min));

        // First-page guarantee: elapsed = u64::MAX always flushes.
        assert!(should_flush_now(3, n, u64::MAX, min));

        // Both arms off when every=0 and time not yet elapsed.
        assert!(!should_flush_now(0, 0, min - 1, min));
        // But time arm still fires even with every=0.
        assert!(should_flush_now(0, 0, min, min));
    }

    #[test]
    fn throttle_window_trips_only_when_full_and_over_threshold() {
        // Not full → never trips, even at 100% transient (short startup burst /
        // small key set can't abort).
        assert!(!throttle_window_tripped(39, 39, 40, 32));
        assert!(!throttle_window_tripped(10, 10, 40, 32));
        // Full but below threshold (e.g. 31/40 transient) → keep going.
        assert!(!throttle_window_tripped(40, 31, 40, 32));
        // Full AND at/over threshold → abort (sustained throttling).
        assert!(throttle_window_tripped(40, 32, 40, 32));
        assert!(throttle_window_tripped(40, 40, 40, 32));
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
    fn delta_reencode_changes_stored_version_even_with_same_logical_content() {
        // THE plumbing test (the bug that served a stale size): the SAME logical
        // file (identical sha/md5/file_size) re-delta'd against a ROTATED
        // reference produces a different .delta blob. The cache token MUST change
        // so the overwrite busts the cache — using the logical md5 (stable here)
        // let it collide and serve the stale size.
        let before = FileMetadata::new_delta(
            "app.zip".into(),
            "LOGICAL_SHA".into(),
            "LOGICAL_MD5".into(), // logical md5 — IDENTICAL before/after
            52_524_126,           // logical size — identical
            "reference.bin".into(),
            "REF_SHA_OLD".into(),
            30_019_443, // delta size against the OLD reference
            None,
        );
        let after = FileMetadata::new_delta(
            "app.zip".into(),
            "LOGICAL_SHA".into(),
            "LOGICAL_MD5".into(),
            52_524_126,
            "reference.bin".into(),
            "REF_SHA_NEW".into(), // rotated reference
            30_244_201,           // different delta bytes
            None,
        );
        let sv_before = ObjState::from_metadata(&before).stored_version;
        let sv_after = ObjState::from_metadata(&after).stored_version;
        assert!(sv_before.is_some());
        assert_ne!(
            sv_before, sv_after,
            "a re-delta against a rotated reference MUST change the stored-version token"
        );
        // And a genuine no-op re-resolve (identical delta) keeps the token stable
        // so the warm-cache hit still works.
        let same = ObjState::from_metadata(&before).stored_version;
        assert_eq!(sv_before, same);
    }

    #[test]
    fn s3_delta_stub_stored_version_folds_size_and_etag() {
        // S3 lite list → delta_stub (ref_sha256 empty). The token must still
        // change on overwrite: fold delta_size with the S3 ETag (carried in md5).
        // Never drop the S3 etag, else two same-size delta blobs would collide.
        use crate::types::StorageInfo;
        let mut m = FileMetadata::new_passthrough(
            "app.zip".into(),
            "LOGICAL_SHA".into(),
            "ETAG_V1".into(), // S3 object etag (lite list puts it in md5)
            30_019_443,
            None,
        );
        m.storage_info = StorageInfo::delta_stub(30_019_443);
        let v1 = ObjState::from_metadata(&m).stored_version.unwrap();

        // Overwrite: new S3 etag (new PUT) — token must change even if size same.
        let mut m2 = m.clone();
        m2.md5 = "ETAG_V2".into();
        let v2 = ObjState::from_metadata(&m2).stored_version.unwrap();
        assert_ne!(
            v1, v2,
            "S3 delta overwrite (new etag) must change the token"
        );
        assert!(v1.contains("30019443") && v1.contains("ETAG_V1"));
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

    /// X-ray H5: an object present on both sides whose HEAD couldn't resolve
    /// must be removed from BOTH maps (the driver does this) so it yields NO
    /// finding. Removing it from only one side — the pre-fix behavior — would
    /// fabricate a missing_on_dest / orphan_on_dest. This asserts the diff
    /// invariant the driver's both-sides removal upholds.
    #[test]
    fn diff_unresolved_key_removed_from_both_sides_is_no_finding() {
        // "b" present on both; simulate the driver dropping it from both after
        // an unresolved HEAD.
        let mut s = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let mut d = s.clone();
        s.remove("b");
        d.remove("b");
        let r = diff_parity(&s, &d);
        assert_eq!(r.matched, 1);
        assert_eq!(
            r.missing_on_dest, 0,
            "no false missing from a both-sides drop"
        );
        assert_eq!(
            r.orphan_on_dest, 0,
            "no false orphan from a both-sides drop"
        );
        // Contrast: a ONE-sided drop (the bug) WOULD have fabricated an orphan.
        let mut s_bug = map(&[
            ("a", st(Some("h"), 1, None, None)),
            ("b", st(Some("h2"), 2, None, None)),
        ]);
        let d_bug = s_bug.clone();
        s_bug.remove("b"); // dropped from source only
        let r_bug = diff_parity(&s_bug, &d_bug);
        assert_eq!(
            r_bug.orphan_on_dest, 1,
            "one-sided drop fabricates a false orphan (the bug this fix prevents)"
        );
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
                etag: etag.clone(),
                multipart_parts: parts,
                created_at: None,
                owned_by_rule: None,
                stored_version: etag,
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
    fn annotate_orphan_is_rule_removable_regardless_of_ownership() {
        use super::super::remediation::{ReasonCode, RerunVerdict};
        // Faithful mirror: even a FOREIGN orphan (written by another tool) is
        // removed by a re-run when mirror-delete is on — ownership no longer
        // downgrades it to a "not ours, manual" finding.
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
            true, // replicate_deletes on
            &HashMap::new(),
            &HashMap::new(),
        );
        let rem = diff.orphan_samples[0].remediation.as_ref().unwrap();
        assert_eq!(rem.reason, ReasonCode::RuleOwnedOrphanSourceDeleted);
        assert_eq!(rem.rerun_helps, RerunVerdict::Yes);
        // A source-absent orphan under mirror-delete is a re-run fix, not a
        // "foreign, hands-off" finding (the ForeignOrphan category is retired).
        assert_eq!(fold_actionable(&diff).rerun_fixes, 1);
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
