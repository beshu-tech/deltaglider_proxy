// SPDX-License-Identifier: GPL-3.0-only

//! Single source of truth for delta-compression savings math.
//!
//! Every place in the codebase that wants to report "how much space did
//! delta compression save?" feeds `FileMetadata` into [`SavingsTotals`]
//! and reads back [`SavingsTotals::savings_percentage`] /
//! [`SavingsTotals::saved_bytes`]. Previously three near-duplicate
//! accumulators (the admin bucket scan, the CLI `stats` subcommand, and
//! the SPA chip) each rolled their own math, and all three undercounted
//! by ignoring the per-deltaspace `reference.bin` bytes — a shared
//! reference is real disk usage, not free.
//!
//! Algorithm
//! ---------
//! For a set of objects (the visible objects under a prefix, an entire
//! bucket, the whole proxy — any scope):
//!
//! * `original_bytes` = sum of the LOGICAL size of every object the user
//!   can see (every `FileMetadata.file_size`, whatever the storage type).
//! * `stored_bytes`   = sum of what actually sits on disk:
//!     - `reference.bin` files contribute `file_size`
//!     - delta files contribute `delta_size` (NOT their logical size)
//!     - passthrough files contribute `file_size`
//!
//! `savings_bytes = max(0, original_bytes - stored_bytes)`.
//! `savings_percentage = savings_bytes / original_bytes`, capped at
//! 99.99% as long as anything was stored. We never display "100.0% saved"
//! because that reads as "the file disappeared"; in practice the
//! reference is always on disk.
//!
//! Important: this module **does not** decide which objects to look at.
//! It's a pure accumulator over what the caller hands it. Callers are
//! responsible for feeding it the right set, including references —
//! `engine.list_objects` hides references so they must be scanned in
//! addition (see `engine::list_deltaspace_references`).

use crate::types::{FileMetadata, StorageInfo};
use serde::Serialize;

/// Aggregated bytes/counts across a scope (prefix, bucket, or proxy-wide).
///
/// Build with [`SavingsTotals::default`] then call
/// [`SavingsTotals::accumulate`] for every [`FileMetadata`] in scope.
#[derive(Debug, Default, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SavingsTotals {
    /// Sum of logical (user-visible) bytes across all objects in scope.
    pub original_bytes: u64,
    /// Sum of on-disk bytes for all storage variants in scope.
    pub stored_bytes: u64,
    /// Bytes occupied by `reference.bin` files in scope.
    pub reference_bytes: u64,
    /// Bytes occupied by `.delta` files in scope (sum of `delta_size`).
    pub delta_stored_bytes: u64,
    /// Bytes occupied by passthrough objects in scope.
    pub passthrough_bytes: u64,
    /// Number of reference baselines.
    pub reference_count: u64,
    /// Number of delta objects.
    pub delta_count: u64,
    /// Number of passthrough objects.
    pub passthrough_count: u64,
}

impl SavingsTotals {
    /// Fold one object's metadata into the running totals. Cheap (O(1));
    /// safe to call in tight loops.
    ///
    /// The caller decides what counts as "in scope" — pass every user-
    /// visible object AND every `reference.bin` whose savings should
    /// count. Forgetting the references is the bug this module exists
    /// to make impossible at the call sites.
    pub fn accumulate(&mut self, meta: &FileMetadata) {
        match &meta.storage_info {
            StorageInfo::Reference { .. } => {
                self.reference_count = self.reference_count.saturating_add(1);
                self.reference_bytes = self.reference_bytes.saturating_add(meta.file_size);
                self.stored_bytes = self.stored_bytes.saturating_add(meta.file_size);
                // Reference.bin is NOT user-visible; it does not count
                // toward `original_bytes`. (Users see deltas, not the
                // reference itself.) Counting it here would inflate the
                // logical-size denominator and bury savings.
            }
            StorageInfo::Delta { delta_size, .. } => {
                self.delta_count = self.delta_count.saturating_add(1);
                self.original_bytes = self.original_bytes.saturating_add(meta.file_size);
                self.delta_stored_bytes = self.delta_stored_bytes.saturating_add(*delta_size);
                self.stored_bytes = self.stored_bytes.saturating_add(*delta_size);
            }
            StorageInfo::Passthrough => {
                self.passthrough_count = self.passthrough_count.saturating_add(1);
                self.passthrough_bytes = self.passthrough_bytes.saturating_add(meta.file_size);
                self.original_bytes = self.original_bytes.saturating_add(meta.file_size);
                self.stored_bytes = self.stored_bytes.saturating_add(meta.file_size);
            }
        }
    }

    /// Saturating "bytes saved" — never negative. Returns 0 when stored
    /// exceeds original (rare: a 99% delta on a 10 MB file still leaves
    /// a 10 MB reference behind, so for a single delta the apparent
    /// savings can be negative).
    pub fn saved_bytes(&self) -> u64 {
        self.original_bytes.saturating_sub(self.stored_bytes)
    }

    /// Signed "bytes saved" — negative when the proxy is storing MORE
    /// bytes than the originals (a real, observable failure mode when
    /// xdelta3 loses or when a 1 KB sentinel has anchored a 10 MB
    /// reference for nothing). The admin "delta efficiency" diagnostic
    /// reports this verbatim; UI surfaces meant for end users should
    /// prefer [`Self::saved_bytes`] which clamps to 0.
    ///
    /// Saturates at `i64::MIN`/`i64::MAX` rather than wrapping when
    /// totals exceed the i64 range. In practice this only kicks in
    /// for buckets past the 8 EB mark — far beyond anything physical
    /// — but the explicit saturation prevents a future refactor from
    /// silently shipping wrap-around UB. Note: JS `number` is precise
    /// only to 2^53 (≈9 PB); past that, the SPA renders an
    /// approximation. See `adminApi.ts::true_savings_bytes` for the
    /// wire-side documentation.
    pub fn saved_bytes_signed(&self) -> i64 {
        // i64::try_from is the explicit saturation primitive: it
        // returns Err on overflow, which we then clamp to i64::MAX.
        // The subtraction itself happens on i128 to avoid the
        // intermediate overflow we'd otherwise hit on `u64 as i64 -
        // u64 as i64` at u64::MAX.
        let diff = self.original_bytes as i128 - self.stored_bytes as i128;
        diff.clamp(i64::MIN as i128, i64::MAX as i128) as i64
    }

    /// Raw compression ratio `1 - stored/original` as a fraction (not
    /// percent), uncapped, signed. `None` when no original bytes are in
    /// scope. Used by diagnostic surfaces that want to show negative
    /// efficiency. User-facing displays must go through
    /// [`Self::savings_percentage`] which clamps the range so
    /// "100% saved" never reaches the UI.
    pub fn compression_ratio(&self) -> Option<f64> {
        if self.original_bytes == 0 {
            return None;
        }
        Some(1.0 - (self.stored_bytes as f64 / self.original_bytes as f64))
    }

    /// Savings percentage 0..=99.99. `None` when there is nothing to
    /// measure (no original bytes). Capped at 99.99 UNCONDITIONALLY
    /// when there is anything to measure — even on the degenerate
    /// `stored_bytes == 0` case. "100% saved" reads as "your data is
    /// gone"; the end-user surface should never see it, even if a
    /// degenerate `delta_size == 0` slips through somewhere upstream.
    ///
    /// Negative ratios are clamped to 0 — they represent "we made it
    /// worse", which is interesting for diagnostics but the end-user
    /// surface should show "no savings yet", not "-12%". The
    /// diagnostic-grade signed value is in [`Self::compression_ratio`].
    pub fn savings_percentage(&self) -> Option<f64> {
        let raw = self.compression_ratio()? * 100.0;
        // Clamp into a sane display range. Top-cap is unconditional:
        // see the doc comment for why "100%" must not reach the UI.
        // The clamp ordering (0.0 floor, 99.99 ceiling) intentionally
        // produces 0 for NaN — `compression_ratio()` itself can't
        // emit NaN here (`original_bytes > 0` enforced upstream) but
        // the floor protects against future refactors.
        Some(raw.clamp(0.0, 99.99))
    }

    /// Number of user-visible objects in scope (deltas + passthroughs).
    pub fn user_visible_count(&self) -> u64 {
        self.delta_count.saturating_add(self.passthrough_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FileMetadata;

    fn reference(size: u64) -> FileMetadata {
        FileMetadata::new_reference(
            "reference.bin".to_string(),
            "source/key".to_string(),
            "deadbeef".to_string(),
            "cafe".to_string(),
            size,
            None,
        )
    }

    fn delta(original: u64, delta: u64) -> FileMetadata {
        FileMetadata::new_delta(
            "v1.zip".to_string(),
            "abcdef".to_string(),
            "feedface".to_string(),
            original,
            "reference.bin".to_string(),
            "refsha".to_string(),
            delta,
            None,
        )
    }

    fn passthrough(size: u64) -> FileMetadata {
        FileMetadata::new_passthrough(
            "video.mp4".to_string(),
            "shashasha".to_string(),
            "md5md5md5".to_string(),
            size,
            None,
        )
    }

    #[test]
    fn empty_totals_report_no_savings() {
        let t = SavingsTotals::default();
        assert_eq!(t.savings_percentage(), None);
        assert_eq!(t.saved_bytes(), 0);
        assert_eq!(t.user_visible_count(), 0);
    }

    #[test]
    fn reference_alone_is_pure_cost() {
        // A bucket with a reference but no deltas yet is overhead with
        // no payoff. `original_bytes` stays at 0 because users don't see
        // the reference, but `stored_bytes` records its cost.
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(10_000));
        assert_eq!(t.original_bytes, 0);
        assert_eq!(t.stored_bytes, 10_000);
        // No denominator: nothing to compute savings against.
        assert_eq!(t.savings_percentage(), None);
    }

    #[test]
    fn one_reference_plus_ten_deltas_reports_honest_savings() {
        // 10 deltas of 100KB each from a 1MB original, sharing a 1MB
        // reference. Stored = 1MB (ref) + 10 * 100KB (deltas) = 2MB.
        // Original (user-visible) = 10 * 1MB = 10MB. Savings = 80%.
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(1_048_576));
        for _ in 0..10 {
            t.accumulate(&delta(1_048_576, 102_400));
        }
        assert_eq!(t.original_bytes, 10 * 1_048_576);
        assert_eq!(
            t.stored_bytes,
            1_048_576 + 10 * 102_400,
            "stored must include reference bytes",
        );
        let pct = t.savings_percentage().expect("non-empty");
        assert!(
            (78.0..=82.0).contains(&pct),
            "expected ~80% savings, got {}",
            pct,
        );
    }

    #[test]
    fn savings_cap_prevents_misleading_100_pct() {
        // A pathological 1-byte delta against a 1 MB reference for 100
        // identical 1 MB files would compute to >99.99% savings; the
        // module clamps so the UI never shows "100% saved" while bytes
        // are on disk.
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(1));
        for _ in 0..1_000 {
            t.accumulate(&delta(1_000_000_000, 1));
        }
        let pct = t.savings_percentage().expect("non-empty");
        assert!(pct <= 99.99, "must clamp at 99.99, got {}", pct);
        assert!(pct > 99.9, "should still be near the cap, got {}", pct);
    }

    #[test]
    fn passthrough_only_reports_zero_savings() {
        // Bucket full of MP4s. Stored == original == sum of sizes; no
        // delta wins to report.
        let mut t = SavingsTotals::default();
        t.accumulate(&passthrough(5_000_000));
        t.accumulate(&passthrough(3_000_000));
        assert_eq!(t.original_bytes, 8_000_000);
        assert_eq!(t.stored_bytes, 8_000_000);
        assert_eq!(t.savings_percentage(), Some(0.0));
        assert_eq!(t.saved_bytes(), 0);
    }

    #[test]
    fn mixed_passthrough_and_delta() {
        // 1 MP4 (5MB passthrough) + 1 reference (10MB) + 1 delta
        // (10MB original / 200KB stored).
        let mut t = SavingsTotals::default();
        t.accumulate(&passthrough(5_000_000));
        t.accumulate(&reference(10_000_000));
        t.accumulate(&delta(10_000_000, 200_000));
        // Original = 5MB (MP4) + 10MB (delta logical) = 15MB.
        // Stored = 5MB + 10MB (reference) + 200KB (delta on disk) = 15.2MB.
        // True savings ≈ -1.3% (delta cost > delta gain because of the
        // big reference for one delta), clamped to 0.
        assert_eq!(t.original_bytes, 15_000_000);
        assert_eq!(t.stored_bytes, 5_000_000 + 10_000_000 + 200_000);
        let pct = t.savings_percentage().expect("non-empty");
        assert_eq!(
            pct, 0.0,
            "negative-savings clamps to 0 (reference cost > delta gain at single-delta scope)",
        );
    }

    #[test]
    fn counts_track_each_kind_separately() {
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(100));
        t.accumulate(&delta(1_000, 50));
        t.accumulate(&delta(1_000, 60));
        t.accumulate(&passthrough(500));
        assert_eq!(t.reference_count, 1);
        assert_eq!(t.delta_count, 2);
        assert_eq!(t.passthrough_count, 1);
        assert_eq!(t.user_visible_count(), 3);
    }

    #[test]
    fn signed_helpers_report_negative_when_proxy_made_it_worse() {
        // 1 KB sentinel pinned a 10 MB reference; subsequent deltas
        // never compressed well. Original = 1 KB, stored = 10 MB +
        // the delta — the proxy is storing MORE than the original.
        // The diagnostic surface needs to see the negative.
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(10_000_000));
        t.accumulate(&delta(1_000, 5_000_000));
        assert_eq!(t.original_bytes, 1_000);
        assert_eq!(t.stored_bytes, 15_000_000);
        // Unsigned helper clamps to 0 (end-user surface).
        assert_eq!(t.saved_bytes(), 0);
        // Signed helper reports the truth: we are 14_999_000 worse off.
        assert_eq!(t.saved_bytes_signed(), 1_000 - 15_000_000);
        // compression_ratio is signed and uncapped; savings_percentage
        // clamps the same value to ≥0 for display.
        let ratio = t.compression_ratio().expect("ratio");
        assert!(
            ratio < 0.0,
            "compression_ratio must report negative, got {ratio}"
        );
        assert_eq!(t.savings_percentage(), Some(0.0));
    }

    /// Regression: a degenerate state where `stored_bytes == 0` but
    /// `original_bytes > 0` (e.g. a `delta_size == 0` ever creeps in
    /// upstream) must still clamp at 99.99 — never display 100% saved.
    /// Pre-fix the cap had a `stored_bytes > 0` guard that allowed
    /// `Some(100.0)` to escape on this corner.
    #[test]
    fn cap_clamps_even_when_stored_is_zero() {
        // stored_bytes left at 0 — the worst-case degenerate.
        let t = SavingsTotals {
            original_bytes: 1_000_000,
            ..SavingsTotals::default()
        };
        let pct = t.savings_percentage().expect("non-empty");
        assert!(pct <= 99.99, "stored=0 must still cap at 99.99 (got {pct})");
        // And the signed compression_ratio is allowed to be exactly
        // 1.0 — diagnostic surfaces aren't user-facing, so the truth
        // is what they want.
        assert_eq!(t.compression_ratio(), Some(1.0));
    }

    /// Regression: `user_visible_count` must NEVER include reference
    /// baselines — they're internal. Without this pin, someone could
    /// "fix" the counter and silently inflate "objects" headline.
    #[test]
    fn user_visible_count_excludes_references() {
        let mut t = SavingsTotals::default();
        t.accumulate(&reference(1_000_000));
        assert_eq!(t.user_visible_count(), 0);
        t.accumulate(&delta(1_000, 50));
        t.accumulate(&passthrough(100));
        assert_eq!(t.user_visible_count(), 2);
        assert_eq!(t.reference_count, 1);
    }

    /// Pin: `saved_bytes_signed` must NOT silently wrap when totals are
    /// in the petabyte range. We use `i64::MIN/2` style values to make
    /// the arithmetic explicit and catch a future refactor that drops
    /// the saturating semantics. Saturates if necessary so the worst
    /// case is bounded.
    #[test]
    fn saved_bytes_signed_saturates_past_i64_range() {
        // u64::MAX original vs zero stored ≈ 18 EB. i64::MAX is half
        // of u64::MAX, so the diff must saturate at i64::MAX rather
        // than wrap to a negative number (the old `as i64`
        // subtraction had this bug latent).
        let t = SavingsTotals {
            original_bytes: u64::MAX,
            stored_bytes: 0,
            ..SavingsTotals::default()
        };
        assert_eq!(
            t.saved_bytes_signed(),
            i64::MAX,
            "must saturate at i64::MAX, not wrap to negative",
        );

        // Inverse: u64::MAX stored vs zero original must saturate at
        // i64::MIN, expressing "the proxy is grossly heavier than
        // what it claims to encode".
        let t = SavingsTotals {
            original_bytes: 0,
            stored_bytes: u64::MAX,
            ..SavingsTotals::default()
        };
        assert_eq!(
            t.saved_bytes_signed(),
            i64::MIN,
            "negative saturation at i64::MIN",
        );

        // Pre-petabyte sanity: a 4 PB savings (well inside i64 +
        // inside JS's 2^53 safe range) must be exact.
        let four_pb = 4 * 1024u64.pow(5);
        let t = SavingsTotals {
            original_bytes: four_pb,
            stored_bytes: 0,
            ..SavingsTotals::default()
        };
        assert_eq!(t.saved_bytes_signed(), four_pb as i64);
    }

    /// Pin: calling `accumulate` twice on the same metadata DOES
    /// double-count. This is the documented contract — callers are
    /// responsible for de-duplication. Pinning it prevents an
    /// "improvement" PR from silently adding a key-set that would
    /// break legitimate loop-over-pages aggregations.
    #[test]
    fn accumulate_is_not_idempotent_by_design() {
        let m = delta(1_000, 50);
        let mut t = SavingsTotals::default();
        t.accumulate(&m);
        t.accumulate(&m);
        assert_eq!(t.delta_count, 2);
        assert_eq!(t.original_bytes, 2_000);
        assert_eq!(t.stored_bytes, 100);
    }

    /// Property test: the savings percentage must always be in
    /// [0.0, 99.99] when there is anything to measure, and must be
    /// finite (never NaN/Inf). Covers the float math under a
    /// pseudo-random sample of byte counts — the integer divide and
    /// cap together should keep the value bounded.
    #[test]
    fn savings_percentage_property_in_range_and_finite() {
        // Deterministic PRNG; reseed if a regression is found.
        let mut state: u64 = 0xDEAD_BEEF_C0FF_EE42;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 11
        };
        for _ in 0..2_000 {
            // stored_bytes can legitimately exceed original (e.g.
            // 1 KB original anchored a 10 MB reference); the helper
            // must still produce a sane displayed value.
            let t = SavingsTotals {
                original_bytes: next(),
                stored_bytes: next(),
                ..SavingsTotals::default()
            };
            if let Some(pct) = t.savings_percentage() {
                assert!(
                    pct.is_finite(),
                    "savings_percentage must be finite (got {pct} from original={}, stored={})",
                    t.original_bytes,
                    t.stored_bytes
                );
                assert!(
                    (0.0..=99.99).contains(&pct),
                    "savings_percentage must be in 0..=99.99 (got {pct} from original={}, stored={})",
                    t.original_bytes,
                    t.stored_bytes
                );
            }
        }
    }
}
