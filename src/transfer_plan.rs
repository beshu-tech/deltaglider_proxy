// SPDX-License-Identifier: GPL-3.0-only

//! Pure decision logic for the streaming multipart transfer path.
//!
//! These functions own the "should we stream this copy, and how do we
//! split it into parts" decisions. They do NO I/O so the whole truth
//! table (gaps/overlaps, min-part, count cap, encryption capability) is
//! unit-tested and proptested without spinning up a backend.
//!
//! Consumed by `transfer.rs` (the copy branch) and `engine/store.rs`
//! (the multipart store). The constants are env-overridable through the
//! `env_parse_with_default` convention.

use crate::config::env_parse_with_default;

const MIB: u64 = 1024 * 1024;

/// S3's hard floor for a non-final multipart part.
pub const S3_MIN_PART_SIZE: u64 = 5 * MIB;

/// S3's hard cap on the number of parts in one multipart upload.
pub const S3_MAX_PARTS: u64 = 10_000;

/// Default size at/above which a passthrough copy uses the streaming
/// multipart path instead of buffering the whole object. 64 MiB.
pub const STREAM_COPY_THRESHOLD: u64 = 64 * MIB;

/// Default multipart part size. 64 MiB → a 29.6 GB object ≈ 463 parts,
/// well under the 10k cap, and a per-part working set of one ranged GET.
pub const MULTIPART_PART_SIZE: u64 = 64 * MIB;

/// Default in-flight parts per object (parallel ranged GET → upload_part).
/// Memory bound becomes O(UPLOAD_CONCURRENCY × part_size).
pub const UPLOAD_CONCURRENCY: usize = 4;

/// Default concurrent objects per replication run (rclone `--transfers`).
pub const TRANSFERS: usize = 4;

/// The storage label a backend reports for whole-object proxy-AES
/// encryption (matches `BackendEncryptionConfig::mode_tag`). Multipart
/// is gated OFF for this label: the `aes-256-gcm-chunked-v1` whole-object
/// GCM framing does not map onto independent S3 parts, so those copies
/// fall back to the existing buffered/chunked path.
pub const PROXY_AES_LABEL: &str = "aes256-gcm-proxy";

/// One contiguous part of a multipart upload (1-indexed part number,
/// inclusive byte range).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartSpan {
    pub number: i32,
    pub start: u64,
    pub end_inclusive: u64,
}

impl PartSpan {
    /// Number of bytes in this part.
    pub fn len(&self) -> u64 {
        self.end_inclusive - self.start + 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Env-resolved stream-copy threshold (`DGP_STREAM_COPY_THRESHOLD`).
pub fn stream_copy_threshold() -> u64 {
    // Floored at 1: threshold 0 would admit a 0-byte object, and plan_parts(0)
    // is empty → a zero-part CompleteMultipartUpload. A 1-byte object still has
    // one part, so `>= 1` keeps empties off the streaming path.
    env_parse_with_default("DGP_STREAM_COPY_THRESHOLD", STREAM_COPY_THRESHOLD).max(1)
}

/// Env-resolved multipart part size (`DGP_MULTIPART_PART_SIZE`), clamped
/// to the S3 minimum so a misconfiguration can't produce illegal parts.
pub fn multipart_part_size() -> u64 {
    env_parse_with_default("DGP_MULTIPART_PART_SIZE", MULTIPART_PART_SIZE).max(S3_MIN_PART_SIZE)
}

/// Env-resolved in-flight parts per object (`DGP_UPLOAD_CONCURRENCY`).
pub fn upload_concurrency() -> usize {
    env_parse_with_default("DGP_UPLOAD_CONCURRENCY", UPLOAD_CONCURRENCY).max(1)
}

/// Env-resolved concurrent objects per run (`DGP_REPLICATION_TRANSFERS`).
pub fn transfers() -> usize {
    env_parse_with_default("DGP_REPLICATION_TRANSFERS", TRANSFERS).max(1)
}

/// True only for a passthrough-labelled object at/above the threshold.
/// Delta/reference objects need full reconstruction and never stream.
pub fn should_stream_copy(size_bytes: u64, storage_label: &str, threshold: u64) -> bool {
    storage_label == "passthrough" && size_bytes >= threshold
}

/// False for whole-object proxy-AES-encrypting backends; true otherwise
/// (plaintext, native SSE-KMS / SSE-S3, none). The proxy-AES branch is
/// routed to the buffered/chunked path by the caller.
pub fn backend_supports_native_multipart(storage_label: &str) -> bool {
    storage_label != PROXY_AES_LABEL
}

/// Split `total` bytes into S3-legal multipart parts of at most
/// `part_size` each:
///   * every part except the last is >= 5 MiB (S3 minimum),
///   * <= 10_000 parts,
///   * contiguous, no gaps or overlaps,
///   * sum of part sizes == total.
///
/// `part_size` is floored at the S3 minimum. If the naive
/// `total / part_size` would exceed 10_000 parts, the part size is grown
/// so the count fits. A `total` of 0 yields an empty Vec (a zero-byte
/// object never reaches the streaming path — it's below the threshold —
/// but the empty plan is the well-defined "no parts to upload" answer).
pub fn plan_parts(total: u64, part_size: u64) -> Vec<PartSpan> {
    if total == 0 {
        return Vec::new();
    }

    let mut part_size = part_size.max(S3_MIN_PART_SIZE);
    // Grow the part size if the count would blow the 10k cap. ceil-div by
    // S3_MAX_PARTS gives the minimum size that keeps us under the cap;
    // re-floor at the S3 minimum (only matters for tiny totals).
    let min_size_for_cap = total.div_ceil(S3_MAX_PARTS);
    if part_size < min_size_for_cap {
        part_size = min_size_for_cap.max(S3_MIN_PART_SIZE);
    }

    let mut parts = Vec::new();
    let mut start = 0u64;
    let mut number: i32 = 1;
    while start < total {
        let mut end_exclusive = start.saturating_add(part_size).min(total);
        // S3 forbids a non-final part below 5 MiB. If the remainder after
        // this part would be a sub-minimum non-final tail, fuse it into
        // this part instead (makes this the last part).
        let remainder = total - end_exclusive;
        if remainder > 0 && remainder < S3_MIN_PART_SIZE {
            end_exclusive = total;
        }
        parts.push(PartSpan {
            number,
            start,
            end_inclusive: end_exclusive - 1,
        });
        start = end_exclusive;
        number += 1;
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn should_stream_only_large_passthrough() {
        assert!(should_stream_copy(
            STREAM_COPY_THRESHOLD,
            "passthrough",
            STREAM_COPY_THRESHOLD
        ));
        assert!(should_stream_copy(
            100 * MIB,
            "passthrough",
            STREAM_COPY_THRESHOLD
        ));
        // below threshold
        assert!(!should_stream_copy(
            STREAM_COPY_THRESHOLD - 1,
            "passthrough",
            STREAM_COPY_THRESHOLD
        ));
        // not passthrough
        assert!(!should_stream_copy(
            100 * MIB,
            "delta",
            STREAM_COPY_THRESHOLD
        ));
        assert!(!should_stream_copy(
            100 * MIB,
            "reference",
            STREAM_COPY_THRESHOLD
        ));
    }

    #[test]
    fn native_multipart_gated_off_for_proxy_aes() {
        assert!(!backend_supports_native_multipart("aes256-gcm-proxy"));
        assert!(backend_supports_native_multipart("none"));
        assert!(backend_supports_native_multipart("sse-kms"));
        assert!(backend_supports_native_multipart("sse-s3"));
        assert!(backend_supports_native_multipart("passthrough"));
    }

    #[test]
    fn plan_parts_simple_even_split() {
        // 192 MiB / 64 MiB = 3 parts exactly.
        let parts = plan_parts(192 * MIB, 64 * MIB);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts[0],
            PartSpan {
                number: 1,
                start: 0,
                end_inclusive: 64 * MIB - 1
            }
        );
        assert_eq!(parts[2].end_inclusive, 192 * MIB - 1);
        assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), 192 * MIB);
    }

    #[test]
    fn plan_parts_fuses_tiny_tail() {
        // 64 MiB + 1 MiB: the 1 MiB tail is sub-minimum, fused into part 1.
        let parts = plan_parts(64 * MIB + MIB, 64 * MIB);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), 64 * MIB + MIB);
    }

    #[test]
    fn plan_parts_large_object_under_cap() {
        // 29.6 GB / 64 MiB ≈ 474 parts — under the cap.
        let total = 29_600 * MIB;
        let parts = plan_parts(total, 64 * MIB);
        assert!(parts.len() as u64 <= S3_MAX_PARTS);
        assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), total);
    }

    #[test]
    fn plan_parts_grows_part_size_to_fit_cap() {
        // A 1 TiB object with a tiny 5 MiB part size would need ~200k parts;
        // the planner must grow the part size to keep <= 10k parts.
        let total = 1024 * 1024 * MIB; // 1 TiB
        let parts = plan_parts(total, S3_MIN_PART_SIZE);
        assert!(parts.len() as u64 <= S3_MAX_PARTS);
        assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), total);
    }

    #[test]
    fn plan_parts_zero_total_is_empty() {
        let parts = plan_parts(0, 64 * MIB);
        assert!(parts.is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn plan_parts_invariants(total in 1u64..(50u64 * 1024 * MIB), raw_part in 1u64..(256 * MIB)) {
            let parts = plan_parts(total, raw_part);
            prop_assert!(!parts.is_empty());

            // count cap
            prop_assert!(parts.len() as u64 <= S3_MAX_PARTS);

            // 1-indexed, contiguous, no gaps/overlaps
            let mut expected_start = 0u64;
            for (i, p) in parts.iter().enumerate() {
                prop_assert_eq!(p.number, (i + 1) as i32);
                prop_assert_eq!(p.start, expected_start);
                prop_assert!(p.end_inclusive >= p.start);
                expected_start = p.end_inclusive + 1;
            }
            // last part ends exactly at total - 1 (reassembled == total)
            prop_assert_eq!(parts.last().unwrap().end_inclusive, total - 1);
            prop_assert_eq!(parts.iter().map(|p| p.len()).sum::<u64>(), total);

            // every part except the last is >= 5 MiB
            for p in &parts[..parts.len().saturating_sub(1)] {
                prop_assert!(p.len() >= S3_MIN_PART_SIZE, "non-final part {} below 5 MiB: {}", p.number, p.len());
            }
        }
    }
}
