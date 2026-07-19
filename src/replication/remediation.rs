// SPDX-License-Identifier: GPL-3.0-only

//! Pure causal model for a parity finding: WHY it diverged, whether a
//! re-run actually fixes it (POLICY-AWARELY), and the guided next step.
//!
//! `analyze_finding` is the decision point — total, no I/O, never panics.
//! The handler does the I/O (ledger join, listing); this module only maps
//! `FindingFacts → Remediation`. The 14-row truth table below has one
//! named unit test per cell plus a proptest pinning the key invariants
//! (notably: SkipIfDestExists + mismatch ⟹ re-run NEVER helps — the lie).

use super::parity::FindingKind;
use super::state_store::ObjectFailure;
use crate::config_sections::ConflictPolicy;
use serde::{Deserialize, Serialize};

/// Inputs to the causal model, reconstructed per finding by the annotator.
#[derive(Debug, Clone)]
pub struct FindingFacts<'a> {
    pub kind: FindingKind,
    pub policy: ConflictPolicy,
    pub replicate_deletes: bool,
    /// Source object creation time (unix seconds), when known.
    pub src_created_at: Option<i64>,
    /// Destination object creation time (unix seconds), when known.
    pub dst_created_at: Option<i64>,
    /// `Some(true)` = dest carries THIS rule's marker, `Some(false)` =
    /// foreign object, `None` = ownership couldn't be determined.
    pub dest_owned_by_rule: Option<bool>,
    /// Per-object failure ledger row, if the key is currently failing.
    pub ledger: Option<&'a ObjectFailure>,
}

/// The diagnosed cause of one finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    NeverCopied,
    CopyFailing,
    SourceModifiedAfterCopy,
    DestModifiedAfterCopy,
    DivergedSameTimestamp,
    DivergedUnknownAge,
    RuleOwnedOrphanSourceDeleted,
}

/// Why a re-run would NOT help.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoReason {
    PolicySkipsExistingDest,
    DestNewerThanSource,
    /// newer-wins can't break a tie: src and dst share the same timestamp but
    /// differ in content, so a re-run neither side wins → skipped.
    TiedTimestampsNoWinner,
    OrphanNeedsDelete,
    CopyKeepsFailing,
}

/// Why a re-run's outcome depends on unknowns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionalReason {
    NewerWinsDependsOnTimestamps,
    /// The last copy failure was a TRANSIENT error (a stalled/slow read, a
    /// timeout, a throttle, a dropped connection). Re-running may well succeed
    /// once the transient condition clears — unlike a structural failure
    /// (permissions, missing bucket) where re-run is futile until fixed.
    TransientCopyErrorMayClear,
}

/// Tri-state verdict — never a bool — on whether re-running the rule fixes
/// this finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum RerunVerdict {
    Yes,
    No { why: NoReason },
    Conditional { why: ConditionalReason },
}

/// The guided fix. Only `RunNow` is executable this iteration; the rest are
/// instructional (see the frontend's `fixActionMeta`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum FixAction {
    RunNow,
    CopyOverwrite,
    DeleteFromDest { foreign: bool },
    ChangeConflictPolicy { to: ConflictPolicy },
    EnableReplicateDeletes,
    ResolveCopyFailure,
    ManualReview,
}

/// The full remediation surfaced per finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remediation {
    pub reason: ReasonCode,
    pub rerun_helps: RerunVerdict,
    pub fix: FixAction,
    pub reason_detail: String,
    pub fix_detail: String,
}

/// True for policies that overwrite an existing dest on copy. SkipIfDestExists
/// is the lone policy that never re-copies a present key. (Used by the proptest
/// invariant; the production analyzer reasons per-arm.)
#[cfg(test)]
fn policy_permits_copy_over_existing(policy: ConflictPolicy) -> bool {
    !matches!(policy, ConflictPolicy::SkipIfDestExists)
}

/// A copy that keeps failing — ledger overrides policy: the bytes never land,
/// so no policy reasoning matters until the underlying error is fixed.
///
/// The re-run verdict depends on the ERROR TYPE. A TRANSIENT error (a stalled
/// or slow read, a timeout, a throttle, a dropped connection) is Conditional —
/// re-running may well succeed once the condition clears (e.g. after raising the
/// read/stall timeouts). A STRUCTURAL error (permissions, missing bucket, quota)
/// is a hard No until the operator fixes the cause. The old code said No for
/// both, which mis-advised operators on transient stalls.
fn copy_failing(ledger: &ObjectFailure) -> Remediation {
    let transient = crate::transfer::is_transient_copy_error(&ledger.last_error);
    let reason_detail = format!(
        "copy has failed {} time(s); last error: {}",
        ledger.consecutive_failures, ledger.last_error
    );
    if transient {
        Remediation {
            reason: ReasonCode::CopyFailing,
            rerun_helps: RerunVerdict::Conditional {
                why: ConditionalReason::TransientCopyErrorMayClear,
            },
            fix: FixAction::ResolveCopyFailure,
            reason_detail,
            fix_detail:
                "transient error (slow/stalled read, timeout, throttle) — re-run to retry; if it \
                 persists, raise the S3 read/stall timeouts (DGP_S3_READ_TIMEOUT_SECS / \
                 DGP_S3_STALL_GRACE_SECS)"
                    .to_string(),
        }
    } else {
        Remediation {
            reason: ReasonCode::CopyFailing,
            rerun_helps: RerunVerdict::No {
                why: NoReason::CopyKeepsFailing,
            },
            fix: FixAction::ResolveCopyFailure,
            reason_detail,
            fix_detail:
                "fix the underlying copy error (permissions, backend reachability), then re-run"
                    .to_string(),
        }
    }
}

/// THE decision point. Total over every `FindingFacts`; never panics.
pub fn analyze_finding(facts: &FindingFacts) -> Remediation {
    match facts.kind {
        FindingKind::MissingOnDest => analyze_missing(facts),
        FindingKind::ChecksumMismatch => analyze_mismatch(facts),
        FindingKind::OrphanOnDest => analyze_orphan(facts),
        // A `Match` never reaches here, but stay total.
        FindingKind::Match => Remediation {
            reason: ReasonCode::NeverCopied,
            rerun_helps: RerunVerdict::No {
                why: NoReason::CopyKeepsFailing,
            },
            fix: FixAction::ManualReview,
            reason_detail: "object already matches".to_string(),
            fix_detail: "no action needed".to_string(),
        },
    }
}

/// Missing is policy-agnostic — the planner always copies a key absent on the
/// dest. The only fork is the failure ledger.
fn analyze_missing(facts: &FindingFacts) -> Remediation {
    if let Some(led) = facts.ledger {
        return copy_failing(led);
    }
    Remediation {
        reason: ReasonCode::NeverCopied,
        rerun_helps: RerunVerdict::Yes,
        fix: FixAction::RunNow,
        reason_detail: "present on source, never copied to destination".to_string(),
        fix_detail: "re-run the rule — a missing key is always copied".to_string(),
    }
}

/// Mismatch is governed by the conflict policy, UNLESS the ledger says the
/// copy keeps failing (ledger overrides policy).
fn analyze_mismatch(facts: &FindingFacts) -> Remediation {
    // Ledger overrides policy: the copy isn't landing, so policy is moot.
    if let Some(led) = facts.ledger {
        return copy_failing(led);
    }
    match facts.policy {
        ConflictPolicy::ContentDiff => Remediation {
            // Content differs and the policy copies on any content difference,
            // so a re-run resolves it.
            reason: ReasonCode::SourceModifiedAfterCopy,
            rerun_helps: RerunVerdict::Yes,
            fix: FixAction::RunNow,
            reason_detail:
                "source and destination differ; content-diff re-copies the changed object"
                    .to_string(),
            fix_detail: "re-run the rule — content-diff copies any object whose bytes differ"
                .to_string(),
        },
        ConflictPolicy::SkipIfDestExists => Remediation {
            // THE LIE, encoded: telling a skip-if-exists user to "just re-run"
            // is wrong — it always skips a present key.
            reason: ReasonCode::DestModifiedAfterCopy,
            rerun_helps: RerunVerdict::No {
                why: NoReason::PolicySkipsExistingDest,
            },
            fix: FixAction::CopyOverwrite,
            reason_detail:
                "source and destination differ, but the rule skips existing destination keys"
                    .to_string(),
            fix_detail:
                "re-run will NOT fix this — overwrite manually, or switch the policy to content-diff"
                    .to_string(),
        },
        ConflictPolicy::NewerWins => analyze_mismatch_newer_wins(facts),
    }
}

/// NewerWins mismatch: the planner copies iff source is strictly newer. Fork
/// on the timestamps (either side unknown ⟹ conditional).
fn analyze_mismatch_newer_wins(facts: &FindingFacts) -> Remediation {
    match (facts.src_created_at, facts.dst_created_at) {
        (Some(s), Some(d)) if s > d => Remediation {
            reason: ReasonCode::SourceModifiedAfterCopy,
            rerun_helps: RerunVerdict::Yes,
            fix: FixAction::RunNow,
            reason_detail: "source is newer than the destination copy".to_string(),
            fix_detail: "re-run the rule — newer-wins copies the newer source".to_string(),
        },
        (Some(s), Some(d)) if d > s => Remediation {
            reason: ReasonCode::DestModifiedAfterCopy,
            rerun_helps: RerunVerdict::No {
                why: NoReason::DestNewerThanSource,
            },
            fix: FixAction::CopyOverwrite,
            reason_detail: "destination is newer than the source — re-run keeps the destination"
                .to_string(),
            fix_detail: "re-run will NOT fix this — overwrite manually if the source should win"
                .to_string(),
        },
        (Some(_), Some(_)) => Remediation {
            // s == d: equal timestamps, diverged content. Newer-wins can't break the tie.
            reason: ReasonCode::DivergedSameTimestamp,
            rerun_helps: RerunVerdict::No {
                why: NoReason::TiedTimestampsNoWinner,
            },
            fix: FixAction::CopyOverwrite,
            reason_detail: "content differs but timestamps are equal — newer-wins won't re-copy"
                .to_string(),
            fix_detail: "re-run will NOT fix this — overwrite manually to force the source"
                .to_string(),
        },
        _ => Remediation {
            // Either timestamp unknown → outcome depends on the real ages.
            reason: ReasonCode::DivergedUnknownAge,
            rerun_helps: RerunVerdict::Conditional {
                why: ConditionalReason::NewerWinsDependsOnTimestamps,
            },
            fix: FixAction::RunNow,
            reason_detail:
                "content differs and an object age is unknown — re-run copies only if source is newer"
                    .to_string(),
            fix_detail: "re-run the rule — it copies only when the source proves newer".to_string(),
        },
    }
}

/// Orphan: a forward copy NEVER deletes. Whether re-run helps depends on
/// ownership (foreign vs ours) and `replicate_deletes`.
fn analyze_orphan(facts: &FindingFacts) -> Remediation {
    // Faithful mirror: an orphan (present on dest, absent at source) is removed
    // by a re-run when mirror-delete is on — regardless of who wrote it. When
    // it's off, enabling it is the fix. Ownership no longer matters.
    if facts.replicate_deletes {
        Remediation {
            reason: ReasonCode::RuleOwnedOrphanSourceDeleted,
            rerun_helps: RerunVerdict::Yes,
            fix: FixAction::DeleteFromDest { foreign: false },
            reason_detail: "present on the destination but absent at source".to_string(),
            fix_detail: "re-run the rule — mirror-delete removes it".to_string(),
        }
    } else {
        Remediation {
            reason: ReasonCode::RuleOwnedOrphanSourceDeleted,
            rerun_helps: RerunVerdict::No {
                why: NoReason::OrphanNeedsDelete,
            },
            fix: FixAction::EnableReplicateDeletes,
            reason_detail: "present on the destination but absent at source; mirror-delete is off"
                .to_string(),
            fix_detail: "enable delete replication on the rule, then re-run to remove it"
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn led(failures: u32) -> ObjectFailure {
        // "boom" is a STRUCTURAL (non-transient) error → hard No.
        ObjectFailure {
            consecutive_failures: failures,
            last_error: "boom".to_string(),
            last_failed_at: 100,
        }
    }

    fn led_err(failures: u32, err: &str) -> ObjectFailure {
        ObjectFailure {
            consecutive_failures: failures,
            last_error: err.to_string(),
            last_failed_at: 100,
        }
    }

    fn facts(kind: FindingKind, policy: ConflictPolicy) -> FindingFacts<'static> {
        FindingFacts {
            kind,
            policy,
            replicate_deletes: false,
            src_created_at: None,
            dst_created_at: None,
            dest_owned_by_rule: None,
            ledger: None,
        }
    }

    // ─────────────── Missing (2 rows) ───────────────

    #[test]
    fn missing_no_ledger_is_never_copied_rerun_fixes() {
        let f = facts(FindingKind::MissingOnDest, ConflictPolicy::NewerWins);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::NeverCopied);
        assert_eq!(r.rerun_helps, RerunVerdict::Yes);
        assert_eq!(r.fix, FixAction::RunNow);
    }

    #[test]
    fn missing_with_ledger_is_copy_failing() {
        let l = led(3);
        let mut f = facts(FindingKind::MissingOnDest, ConflictPolicy::NewerWins);
        f.ledger = Some(&l);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::CopyFailing);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::CopyKeepsFailing
            }
        );
        assert_eq!(r.fix, FixAction::ResolveCopyFailure);
        assert!(r.reason_detail.contains('3'));
    }

    // ─────────────── Mismatch (8 rows) ───────────────

    #[test]
    fn mismatch_content_diff_reruns() {
        let f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::ContentDiff);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::SourceModifiedAfterCopy);
        assert_eq!(r.rerun_helps, RerunVerdict::Yes);
        assert_eq!(r.fix, FixAction::RunNow);
    }

    #[test]
    fn mismatch_skip_if_dest_exists_is_the_lie() {
        let f = facts(
            FindingKind::ChecksumMismatch,
            ConflictPolicy::SkipIfDestExists,
        );
        let r = analyze_finding(&f);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::PolicySkipsExistingDest
            }
        );
        assert_eq!(r.fix, FixAction::CopyOverwrite);
    }

    #[test]
    fn mismatch_newer_wins_src_newer_reruns() {
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::NewerWins);
        f.src_created_at = Some(200);
        f.dst_created_at = Some(100);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::SourceModifiedAfterCopy);
        assert_eq!(r.rerun_helps, RerunVerdict::Yes);
        assert_eq!(r.fix, FixAction::RunNow);
    }

    #[test]
    fn mismatch_newer_wins_dst_newer_does_not_rerun() {
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::NewerWins);
        f.src_created_at = Some(100);
        f.dst_created_at = Some(200);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::DestModifiedAfterCopy);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::DestNewerThanSource
            }
        );
        assert_eq!(r.fix, FixAction::CopyOverwrite);
    }

    #[test]
    fn mismatch_newer_wins_same_timestamp_diverged() {
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::NewerWins);
        f.src_created_at = Some(150);
        f.dst_created_at = Some(150);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::DivergedSameTimestamp);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::TiedTimestampsNoWinner
            }
        );
        assert_eq!(r.fix, FixAction::CopyOverwrite);
    }

    #[test]
    fn mismatch_newer_wins_unknown_age_is_conditional() {
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::NewerWins);
        f.src_created_at = Some(150);
        f.dst_created_at = None;
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::DivergedUnknownAge);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::Conditional {
                why: ConditionalReason::NewerWinsDependsOnTimestamps
            }
        );
        assert_eq!(r.fix, FixAction::RunNow);
    }

    #[test]
    fn mismatch_with_ledger_is_copy_failing_overrides_policy() {
        // Even under ContentDiff (a copy-permitting policy), a failing ledger
        // wins: the bytes never land.
        let l = led(5);
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::ContentDiff);
        f.ledger = Some(&l);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::CopyFailing);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::CopyKeepsFailing
            }
        );
        assert_eq!(r.fix, FixAction::ResolveCopyFailure);
    }

    #[test]
    fn mismatch_skip_with_ledger_is_still_copy_failing() {
        // Ledger overrides policy even for the skip case — distinct 8th row.
        let l = led(2);
        let mut f = facts(
            FindingKind::ChecksumMismatch,
            ConflictPolicy::SkipIfDestExists,
        );
        f.ledger = Some(&l);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::CopyFailing);
        assert_eq!(r.fix, FixAction::ResolveCopyFailure);
    }

    #[test]
    fn copy_failing_transient_error_is_conditional_not_hard_no() {
        // A TRANSIENT last-error (a stalled/slow read → "streaming error") means
        // re-run MAY succeed once the condition clears — Conditional, not a hard
        // No. This is the prod case: slow Hetzner reads false-aborting.
        let l = led_err(3, "source retrieve failed: S3 error: streaming error");
        let mut f = facts(FindingKind::ChecksumMismatch, ConflictPolicy::ContentDiff);
        f.ledger = Some(&l);
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::CopyFailing);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::Conditional {
                why: ConditionalReason::TransientCopyErrorMayClear
            }
        );

        // A STRUCTURAL error stays a hard No.
        let l2 = led_err(3, "AccessDenied: permission denied");
        f.ledger = Some(&l2);
        let r2 = analyze_finding(&f);
        assert_eq!(
            r2.rerun_helps,
            RerunVerdict::No {
                why: NoReason::CopyKeepsFailing
            }
        );
    }

    // ─────────────── Orphan (4 rows) ───────────────

    #[test]
    fn orphan_rule_owned_with_deletes_reruns() {
        let mut f = facts(FindingKind::OrphanOnDest, ConflictPolicy::NewerWins);
        f.dest_owned_by_rule = Some(true);
        f.replicate_deletes = true;
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::RuleOwnedOrphanSourceDeleted);
        assert_eq!(r.rerun_helps, RerunVerdict::Yes);
        assert_eq!(r.fix, FixAction::DeleteFromDest { foreign: false });
    }

    #[test]
    fn orphan_rule_owned_no_deletes_needs_enable() {
        let mut f = facts(FindingKind::OrphanOnDest, ConflictPolicy::NewerWins);
        f.dest_owned_by_rule = Some(true);
        f.replicate_deletes = false;
        let r = analyze_finding(&f);
        assert_eq!(r.reason, ReasonCode::RuleOwnedOrphanSourceDeleted);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::OrphanNeedsDelete
            }
        );
        assert_eq!(r.fix, FixAction::EnableReplicateDeletes);
    }

    #[test]
    fn orphan_foreign_with_deletes_reruns_too() {
        // Faithful mirror: a foreign orphan is deleted by a re-run just like an
        // owned one — ownership no longer gates the delete.
        let mut f = facts(FindingKind::OrphanOnDest, ConflictPolicy::NewerWins);
        f.dest_owned_by_rule = Some(false);
        f.replicate_deletes = true;
        let r = analyze_finding(&f);
        assert_eq!(r.rerun_helps, RerunVerdict::Yes);
        assert_eq!(r.fix, FixAction::DeleteFromDest { foreign: false });
    }

    #[test]
    fn orphan_unknown_ownership_with_deletes_off_needs_enable() {
        // Ownership unknown, mirror-delete off → enable it, then re-run.
        let mut f = facts(FindingKind::OrphanOnDest, ConflictPolicy::NewerWins);
        f.dest_owned_by_rule = None;
        f.replicate_deletes = false;
        let r = analyze_finding(&f);
        assert_eq!(
            r.rerun_helps,
            RerunVerdict::No {
                why: NoReason::OrphanNeedsDelete
            }
        );
        assert_eq!(r.fix, FixAction::EnableReplicateDeletes);
    }

    // ─────────────── proptest invariants ───────────────

    fn arb_kind() -> impl Strategy<Value = FindingKind> {
        prop_oneof![
            Just(FindingKind::MissingOnDest),
            Just(FindingKind::ChecksumMismatch),
            Just(FindingKind::OrphanOnDest),
            Just(FindingKind::Match),
        ]
    }

    fn arb_policy() -> impl Strategy<Value = ConflictPolicy> {
        prop_oneof![
            Just(ConflictPolicy::NewerWins),
            Just(ConflictPolicy::ContentDiff),
            Just(ConflictPolicy::SkipIfDestExists),
        ]
    }

    proptest! {
        #[test]
        fn never_panics_and_invariants_hold(
            kind in arb_kind(),
            policy in arb_policy(),
            rd in any::<bool>(),
            src in prop::option::of(0i64..1000),
            dst in prop::option::of(0i64..1000),
            owned in prop::option::of(any::<bool>()),
            has_ledger in any::<bool>(),
        ) {
            let l = led(4);
            let f = FindingFacts {
                kind,
                policy,
                replicate_deletes: rd,
                src_created_at: src,
                dst_created_at: dst,
                dest_owned_by_rule: owned,
                ledger: has_ledger.then_some(&l),
            };
            let r = analyze_finding(&f);

            // (b) Yes ⟹ fix ∈ {RunNow, DeleteFromDest{foreign:false}}.
            if r.rerun_helps == RerunVerdict::Yes {
                let ok = matches!(
                    r.fix,
                    FixAction::RunNow | FixAction::DeleteFromDest { foreign: false }
                );
                prop_assert!(ok);
            }

            // (c) SK + mismatch ⟹ No ALWAYS (the lie, provably). Holds even
            // with a ledger (CopyKeepsFailing is also a No-reason).
            if kind == FindingKind::ChecksumMismatch
                && policy == ConflictPolicy::SkipIfDestExists
            {
                let is_no = matches!(r.rerun_helps, RerunVerdict::No { .. });
                prop_assert!(is_no);
            }

            // (d) faithful mirror: an orphan's verdict depends ONLY on
            // replicate_deletes, never on ownership. deletes on ⟹ Yes (re-run
            // removes it); deletes off ⟹ No{OrphanNeedsDelete} (enable it).
            if kind == FindingKind::OrphanOnDest {
                if rd {
                    prop_assert_eq!(r.rerun_helps, RerunVerdict::Yes);
                    prop_assert_eq!(r.fix, FixAction::DeleteFromDest { foreign: false });
                } else {
                    prop_assert_eq!(
                        r.rerun_helps,
                        RerunVerdict::No { why: NoReason::OrphanNeedsDelete }
                    );
                }
            }

            // (e) (missing|mismatch) + ledger + copy-permitting policy ⟹ CopyFailing.
            let copy_permitting = policy_permits_copy_over_existing(policy);
            if has_ledger
                && copy_permitting
                && matches!(
                    kind,
                    FindingKind::MissingOnDest | FindingKind::ChecksumMismatch
                )
            {
                prop_assert_eq!(r.reason, ReasonCode::CopyFailing);
            }
        }
    }
}
