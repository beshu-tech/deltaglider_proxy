// SPDX-License-Identifier: GPL-3.0-only

//! S3-implementation fingerprinting from HTTP response headers.
//!
//! Identifies which S3-compatible server is behind an opaque endpoint (a custom
//! domain, a self-hosted box) so operators get a named platform in logs and so
//! we can emit a better error when a backend lacks a needed capability.
//!
//! This is PURE and advisory. It does NOT decide capabilities — conditional-write
//! support is version-gated (an old MinIO self-IDs as MinIO but still 501s a
//! conditional PUT), so capability is settled by an actual PROBE
//! (`StorageBackend::supports_conditional_writes`), never by inference here. The
//! platform is a telemetry label and a hint for human-readable diagnostics.
//!
//! Fingerprints are grounded in MEASURED responses (2026-07-03) from real
//! backends, not vendor docs — see `docs/plan/coordination-cas-design.md`.

/// A recognised S3-compatible implementation. `Unknown` carries the raw `Server`
/// header (if any) so an unrecognised platform still surfaces something useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Platform {
    Aws,
    MinIO,
    CephRgw,
    Backblaze,
    CloudflareR2,
    Wasabi,
    SeaweedFs,
    Garage,
    /// Unrecognised — carries the `Server` header value if one was present.
    Unknown(Option<String>),
}

impl S3Platform {
    /// Stable lowercase id for logs/metrics labels.
    pub fn id(&self) -> &str {
        match self {
            S3Platform::Aws => "aws",
            S3Platform::MinIO => "minio",
            S3Platform::CephRgw => "ceph-rgw",
            S3Platform::Backblaze => "backblaze-b2",
            S3Platform::CloudflareR2 => "cloudflare-r2",
            S3Platform::Wasabi => "wasabi",
            S3Platform::SeaweedFs => "seaweedfs",
            S3Platform::Garage => "garage",
            S3Platform::Unknown(_) => "unknown",
        }
    }

    /// Human-readable name for diagnostics.
    pub fn display_name(&self) -> String {
        match self {
            S3Platform::Aws => "AWS S3".into(),
            S3Platform::MinIO => "MinIO".into(),
            S3Platform::CephRgw => "Ceph RadosGW".into(),
            S3Platform::Backblaze => "Backblaze B2".into(),
            S3Platform::CloudflareR2 => "Cloudflare R2".into(),
            S3Platform::Wasabi => "Wasabi".into(),
            S3Platform::SeaweedFs => "SeaweedFS".into(),
            S3Platform::Garage => "Garage".into(),
            S3Platform::Unknown(Some(s)) => format!("unknown (Server: {s})"),
            S3Platform::Unknown(None) => "unknown".into(),
        }
    }

    /// The vendor's DOCUMENTED conditional-write support, as a HINT only. `None`
    /// means "no strong prior — probe it". This is deliberately conservative:
    /// it never returns `Some(true)` for a self-hostable impl (MinIO/Ceph/Seaweed/
    /// Garage) because those are version-gated and only the probe is authoritative.
    /// It exists to (a) explain a probe failure ("this looks like B2, which…") and
    /// (b) let a caller skip the probe for a cloud vendor with a fixed answer.
    pub fn conditional_write_hint(&self) -> Option<bool> {
        match self {
            // Cloud vendors with a fixed, known answer — safe to trust.
            S3Platform::Aws | S3Platform::CloudflareR2 => Some(true),
            S3Platform::Backblaze => Some(false), // measured: 501 Not Implemented
            // Self-hostable / version-gated — no prior, must probe.
            S3Platform::MinIO
            | S3Platform::CephRgw
            | S3Platform::Wasabi
            | S3Platform::SeaweedFs
            | S3Platform::Garage
            | S3Platform::Unknown(_) => None,
        }
    }
}

/// A minimal, borrowed view of the response headers we fingerprint on. Keeping
/// this a plain struct (rather than a `HeaderMap`) makes `detect_platform` a pure
/// function that's trivial to unit-test against captured header sets.
#[derive(Debug, Default, Clone)]
pub struct ResponseSignals<'a> {
    /// `Server` response header, if present.
    pub server: Option<&'a str>,
    /// `x-amz-request-id`, if present.
    pub request_id: Option<&'a str>,
    /// Whether `x-amz-id-2` was present (its mere presence is a tell).
    pub has_amz_id_2: bool,
    /// `Content-Type` of the response (B2 answers XML errors with json ctype).
    pub content_type: Option<&'a str>,
}

/// Fingerprint an S3 implementation from one response's signals.
///
/// Order matters: the strongest, least-spoofable tells first (`Server` for the
/// impls that set a distinctive one), then request-id SHAPE, then softer combos.
pub fn detect_platform(sig: &ResponseSignals<'_>) -> S3Platform {
    // 1. Server header — the direct tell for the impls that set a distinctive one.
    if let Some(server) = sig.server {
        let s = server.trim();
        let low = s.to_ascii_lowercase();
        // Exact/word matches first so "nginx" (B2's fronting proxy) doesn't
        // shadow a more specific downstream signal.
        if low == "amazons3" {
            return S3Platform::Aws;
        }
        if low.contains("minio") {
            return S3Platform::MinIO;
        }
        if low.contains("seaweedfs") {
            return S3Platform::SeaweedFs;
        }
        if low.contains("garage") {
            return S3Platform::Garage;
        }
        if low.contains("cloudflare") {
            return S3Platform::CloudflareR2;
        }
        if low.contains("wasabi") {
            return S3Platform::Wasabi;
        }
        // `Server: nginx` + short 16-hex request-id + x-amz-id-2 present is the
        // measured Backblaze-B2 shape (B2 fronts its S3 API with nginx).
        if low == "nginx"
            && sig.has_amz_id_2
            && sig.request_id.map(is_short_hex_id).unwrap_or(false)
        {
            return S3Platform::Backblaze;
        }
        // Known Server header but not matched above → fall through to shape tells,
        // remembering the raw value for the Unknown case.
    }

    // 2. Ceph RadosGW: no Server header, but its x-amz-request-id is the
    //    unmistakable `tx<hex>-<epoch>-<hex>-<zone>` transaction id, frequently
    //    ending in the cluster name (e.g. `-hel1-prod1-ceph4`).
    if let Some(rid) = sig.request_id {
        if is_ceph_txn_id(rid) {
            return S3Platform::CephRgw;
        }
    }

    // 3. AWS without a Server match: long base64-ish x-amz-id-2 + a request-id.
    if sig.has_amz_id_2 {
        if let Some(rid) = sig.request_id {
            // B2 already handled above (needs Server: nginx). A long id-2 with a
            // non-short request id is AWS-shaped.
            if !is_short_hex_id(rid) {
                return S3Platform::Aws;
            }
        }
    }

    S3Platform::Unknown(sig.server.map(|s| s.to_string()))
}

/// Ceph RGW transaction id: starts with `tx`, then hex, dashes, and (usually) a
/// trailing zone/cluster token. We match the structural prefix, not the exact
/// suffix (which varies per deployment).
fn is_ceph_txn_id(rid: &str) -> bool {
    let r = rid.trim();
    // `tx` + at least one hex char + a dash somewhere (the segmented shape).
    if !(r.starts_with("tx") || r.starts_with("TX")) {
        return false;
    }
    let rest = &r[2..];
    rest.contains('-')
        && rest
            .chars()
            .take_while(|c| *c != '-')
            .all(|c| c.is_ascii_hexdigit())
        && rest.chars().take_while(|c| *c != '-').count() >= 4
}

/// A "short hex id" = the ~16-char lowercase hex request id used by Backblaze B2
/// (and, upper-cased, by MinIO). Distinct from AWS's much longer ids and from
/// Ceph's `tx…` transaction ids.
fn is_short_hex_id(rid: &str) -> bool {
    let r = rid.trim();
    (12..=20).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig<'a>(server: Option<&'a str>, rid: Option<&'a str>, id2: bool) -> ResponseSignals<'a> {
        ResponseSignals {
            server,
            request_id: rid,
            has_amz_id_2: id2,
            content_type: None,
        }
    }

    #[test]
    fn minio_by_server_header() {
        // measured: Server: MinIO, 16 upper-hex request id, no x-amz-id-2
        let s = sig(Some("MinIO"), Some("18BEBD09DFEB821C"), false);
        assert_eq!(detect_platform(&s), S3Platform::MinIO);
        assert_eq!(detect_platform(&s).conditional_write_hint(), None); // must probe
    }

    #[test]
    fn ceph_by_transaction_id() {
        // measured Hetzner: no Server, tx…-hel1-prod1-ceph4, no id-2
        let s = sig(
            None,
            Some("tx0000078106eb39d679dc8-006a477e7d-112d8446-hel1-prod1-ceph4"),
            false,
        );
        assert_eq!(detect_platform(&s), S3Platform::CephRgw);
    }

    #[test]
    fn backblaze_by_nginx_plus_id2_plus_short_id() {
        // measured: Server: nginx, 16 lower-hex request id, x-amz-id-2 present
        let s = sig(Some("nginx"), Some("743aac6d71b58dc0"), true);
        assert_eq!(detect_platform(&s), S3Platform::Backblaze);
        // B2 has a fixed, measured answer — no probe needed.
        assert_eq!(detect_platform(&s).conditional_write_hint(), Some(false));
    }

    #[test]
    fn aws_by_server_and_long_id2() {
        assert_eq!(
            detect_platform(&sig(Some("AmazonS3"), None, true)),
            S3Platform::Aws
        );
        // no Server, long id-2, long request id
        let s = sig(None, Some("RG9PZ3JhbmRvbUFXU3JlcXVlc3RpZA"), true);
        assert_eq!(detect_platform(&s), S3Platform::Aws);
    }

    #[test]
    fn seaweedfs_and_garage_by_server() {
        assert_eq!(
            detect_platform(&sig(Some("SeaweedFS"), None, false)),
            S3Platform::SeaweedFs
        );
        assert_eq!(
            detect_platform(&sig(Some("Garage"), None, false)),
            S3Platform::Garage
        );
    }

    #[test]
    fn nginx_alone_is_not_backblaze() {
        // A bare nginx front without the id-2 + short-hex combo stays Unknown —
        // we don't want to mislabel arbitrary nginx-fronted S3 as B2.
        let s = sig(Some("nginx"), None, false);
        assert_eq!(
            detect_platform(&s),
            S3Platform::Unknown(Some("nginx".into()))
        );
    }

    #[test]
    fn unknown_carries_server_header() {
        let s = sig(Some("SomeNewThing/1.0"), None, false);
        assert_eq!(
            detect_platform(&s),
            S3Platform::Unknown(Some("SomeNewThing/1.0".into()))
        );
    }

    #[test]
    fn empty_signals_are_unknown_none() {
        assert_eq!(
            detect_platform(&sig(None, None, false)),
            S3Platform::Unknown(None)
        );
    }

    #[test]
    fn ceph_txn_id_shape_guard() {
        assert!(is_ceph_txn_id("tx00007-abc-def-zone"));
        assert!(!is_ceph_txn_id("tx-nohex")); // no hex run before first dash
        assert!(!is_ceph_txn_id("18BEBD09DFEB821C")); // MinIO id, not tx…
        assert!(!is_ceph_txn_id("txab")); // too short, no dash
    }
}
