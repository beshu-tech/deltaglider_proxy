// SPDX-License-Identifier: GPL-3.0-only

//! Pure security primitives shared across auth and admin surfaces.
//!
//! Everything here is a pure function — no I/O, no global state — so each
//! check has a unit-testable truth table and lives outside the request
//! pipeline. Wire-up happens at the call site.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What an outbound URL is going to be used for. Drives policy:
/// production callers (`Backend`, `Oidc`, `Webhook`) require HTTPS and
/// reject private address ranges; `BackendDev` keeps the door open for
/// local MinIO / dev containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    /// Backend S3 endpoint. Restrictive: HTTPS required, no private IPs.
    Backend,
    /// Like Backend but allows http:// + private IPs. Set explicitly when
    /// the operator opts into a dev/CI deployment with MinIO on localhost.
    BackendDev,
    /// OIDC issuer / JWKS / token URL. HTTPS required, no private IPs.
    Oidc,
    /// Outbound webhook target. Same policy as OIDC.
    Webhook,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UrlValidationError {
    #[error("URL is empty")]
    Empty,
    #[error("URL is not a valid absolute URL: {0}")]
    Parse(String),
    #[error("URL scheme '{0}' is not allowed (use https://)")]
    BadScheme(String),
    #[error("URL is missing a host")]
    NoHost,
    #[error("URL host '{0}' is a literal IP in a forbidden range (loopback / link-local / private / cloud metadata)")]
    ForbiddenIp(String),
    #[error("URL host '{0}' resolves to a name we won't trust (e.g. 'localhost', '*.internal')")]
    ForbiddenHost(String),
}

/// Validate an outbound URL. Pure function: no DNS resolution. We reject
/// **literal-IP** hosts that fall in forbidden ranges, plus a small set
/// of well-known hostnames (`localhost`, `metadata.google.internal`, …).
///
/// **Important**: this does NOT resolve DNS. A hostile DNS A record
/// pointing `legit.example.com` at `169.254.169.254` would still pass.
/// That's a DNS-rebinding concern that needs a connect-time hook in the
/// HTTP client (`reqwest::redirect::Policy::custom` + a resolver wrapper).
/// This function is the cheap first line; the caller is expected to
/// pair it with `redirect(Policy::none())` and, where feasible, a
/// resolver hook for follow-up defence.
pub fn validate_outbound_url(url: &str, kind: UrlKind) -> Result<(), UrlValidationError> {
    if url.is_empty() {
        return Err(UrlValidationError::Empty);
    }

    let parsed = reqwest::Url::parse(url).map_err(|e| UrlValidationError::Parse(e.to_string()))?;

    let scheme = parsed.scheme();
    let allow_http = matches!(kind, UrlKind::BackendDev);
    let allowed = if allow_http {
        matches!(scheme, "https" | "http")
    } else {
        scheme == "https"
    };
    if !allowed {
        return Err(UrlValidationError::BadScheme(scheme.to_string()));
    }

    let host = parsed.host_str().ok_or(UrlValidationError::NoHost)?;
    check_host(host, kind)
}

fn check_host(host: &str, kind: UrlKind) -> Result<(), UrlValidationError> {
    let normalised = host.trim_matches(['[', ']']).to_ascii_lowercase();

    if FORBIDDEN_HOSTNAMES.iter().any(|h| normalised == *h)
        || FORBIDDEN_SUFFIXES.iter().any(|s| normalised.ends_with(s))
    {
        // BackendDev permits `localhost` (and only that — not `metadata.*`).
        let dev_ok =
            matches!(kind, UrlKind::BackendDev) && DEV_ALLOWED.iter().any(|h| normalised == *h);
        if !dev_ok {
            return Err(UrlValidationError::ForbiddenHost(host.to_string()));
        }
    }

    if let Ok(ip) = normalised.parse::<IpAddr>() {
        if !ip_is_acceptable(ip, kind) {
            return Err(UrlValidationError::ForbiddenIp(host.to_string()));
        }
    }

    Ok(())
}

/// Bucket-name policy: reject names that parse as an IP in any common
/// dotted notation. AWS S3 rejects all IP-like bucket names; we need
/// parity so an operator can't break downstream client SSRF heuristics
/// by creating a `127.1` or `0x7f.0.0.1` bucket.
///
/// **Note**: we do NOT flag single-token decimal/hex forms (e.g.
/// `2130706433`, `0x7f000001`). They're technically an IP encoding,
/// but they overlap heavily with legitimate numeric bucket names
/// (`0400`, `123`, `000`) — and our outbound-URL guard already
/// covers the SSRF surface these would otherwise feed.
pub fn bucket_name_is_ip_like(name: &str) -> bool {
    if name.parse::<IpAddr>().is_ok() {
        return true;
    }
    // Permissive dotted parser: accepts radix-tagged segments
    // (0xNN / 0NN / decimal) — covers `0x7f.0.0.1`, `0177.0.0.1`,
    // `127.1` (BSD shorthand), etc. We require at least one '.' to
    // avoid the single-token bucket-name collision.
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() == 2 || parts.len() == 4 {
        let parsed: Option<Vec<u64>> = parts.iter().map(|seg| parse_ip_segment(seg)).collect();
        if let Some(v) = parsed {
            if v.iter().all(|&n| n <= 0xFFFF_FFFF) {
                return true;
            }
        }
    }
    false
}

fn parse_ip_segment(seg: &str) -> Option<u64> {
    if seg.is_empty() {
        return None;
    }
    if let Some(rest) = seg.strip_prefix("0x").or_else(|| seg.strip_prefix("0X")) {
        return u64::from_str_radix(rest, 16).ok();
    }
    if seg.starts_with('0') && seg.len() > 1 {
        return u64::from_str_radix(seg, 8).ok();
    }
    seg.parse::<u64>().ok()
}

/// Hosts we never let outbound traffic target unless the caller is
/// `BackendDev`-flagged and the host is also in [`DEV_ALLOWED`].
const FORBIDDEN_HOSTNAMES: &[&str] = &[
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "metadata.google.internal",
    "metadata",
    "metadata.aws",
];

const FORBIDDEN_SUFFIXES: &[&str] = &[".internal", ".local", ".localdomain"];

const DEV_ALLOWED: &[&str] = &["localhost", "ip6-localhost", "ip6-loopback"];

fn ip_is_acceptable(ip: IpAddr, kind: UrlKind) -> bool {
    // Cloud instance-metadata services are NEVER acceptable, even in
    // BackendDev mode — pointing the S3 backend at IMDS is the cloud-
    // takeover pivot we're explicitly blocking, and it's never a
    // legitimate dev use case.
    if ip_is_metadata_service(ip) {
        return false;
    }
    let private = match ip {
        IpAddr::V4(v4) => ipv4_is_private(v4),
        IpAddr::V6(v6) => ipv6_is_private(v6),
    };
    if !private {
        return true;
    }
    // Other private IPs accepted only for BackendDev (operator-opted-in).
    matches!(kind, UrlKind::BackendDev)
}

fn ip_is_metadata_service(ip: IpAddr) -> bool {
    match ip {
        // AWS IMDSv1/IMDSv2, Azure IMDS, GCP metadata server (all same v4).
        IpAddr::V4(v4) => v4.octets() == [169, 254, 169, 254],
        // IPv4-mapped form.
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(|m| m.octets() == [169, 254, 169, 254])
            .unwrap_or(false),
    }
}

fn ipv4_is_private(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_private()
        // 100.64.0.0/10 — CGNAT (RFC 6598)
        || (o[0] == 100 && (o[1] & 0xC0) == 64)
        // 192.0.0.0/24 — IETF reserved
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15 — benchmark
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
}

fn ipv6_is_private(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 — unique local
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        // fe80::/10 — link-local
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        // IPv4-mapped (::ffff:0:0/96) — reject; let the IPv4 path handle it
        || ip.to_ipv4_mapped().is_some()
}

/// Hard-coded allowlist of JWT signing algorithms we accept. RFC 7518
/// names; rejects `none`, HS256/384/512 (HMAC — symmetric key-confusion),
/// and any future algorithm we haven't reviewed.
pub fn jwt_alg_is_allowed(alg: jsonwebtoken::Algorithm) -> bool {
    use jsonwebtoken::Algorithm::*;
    matches!(
        alg,
        RS256 | RS384 | RS512 | ES256 | ES384 | PS256 | PS384 | PS512
    )
}

/// Public-prefix policy: a non-empty prefix MUST end in `/`. Empty
/// string means "the entire bucket is public" (the existing
/// `public: true` shorthand). Anything in between (e.g. "builds" with
/// no slash) is the operator-misconfig that exposes
/// `builds-internal/secret.zip`.
pub fn validate_public_prefix(prefix: &str) -> Result<(), &'static str> {
    if prefix.is_empty() {
        return Ok(());
    }
    if prefix.contains("..") || prefix.contains('\0') || prefix.contains("//") {
        return Err("prefix must not contain '..', NUL, or '//'");
    }
    if !prefix.ends_with('/') {
        return Err("non-empty public_prefix must end in '/'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_url_blocks_imds() {
        for u in [
            "http://169.254.169.254/latest/meta-data/",
            "https://169.254.169.254/",
            "https://[::ffff:169.254.169.254]/",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::Backend).is_err(),
                "should reject IMDS: {u}"
            );
        }
    }

    #[test]
    fn validate_url_blocks_loopback_private_link_local_cgnat() {
        let cases = [
            "https://127.0.0.1/",
            "https://10.0.0.1/",
            "https://172.16.0.1/",
            "https://192.168.0.1/",
            "https://100.64.0.1/",
            "https://0.0.0.0/",
            "https://[::1]/",
            "https://[fe80::1]/",
            "https://[fc00::1]/",
        ];
        for u in cases {
            assert!(
                validate_outbound_url(u, UrlKind::Backend).is_err(),
                "should reject: {u}"
            );
        }
    }

    #[test]
    fn validate_url_blocks_metadata_hostnames() {
        for u in [
            "https://metadata.google.internal/",
            "https://anything.internal/",
            "https://foo.local/",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::Oidc).is_err(),
                "should reject: {u}"
            );
        }
    }

    #[test]
    fn validate_url_rejects_http_in_strict_mode() {
        assert!(validate_outbound_url("http://example.com/", UrlKind::Backend).is_err());
        assert!(validate_outbound_url("http://example.com/", UrlKind::Oidc).is_err());
        assert!(validate_outbound_url("http://example.com/", UrlKind::Webhook).is_err());
    }

    #[test]
    fn validate_url_accepts_http_for_backend_dev() {
        assert!(validate_outbound_url("http://localhost:9000/", UrlKind::BackendDev).is_ok());
        assert!(validate_outbound_url("http://127.0.0.1:9000/", UrlKind::BackendDev).is_ok());
    }

    #[test]
    fn validate_url_accepts_legitimate_public_targets() {
        for u in [
            "https://s3.amazonaws.com/",
            "https://s3.eu-central-1.amazonaws.com/",
            "https://accounts.google.com/",
            "https://login.microsoftonline.com/common/v2.0",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::Oidc).is_ok(),
                "should accept: {u}"
            );
        }
    }

    #[test]
    fn validate_url_rejects_garbage() {
        assert!(matches!(
            validate_outbound_url("", UrlKind::Backend),
            Err(UrlValidationError::Empty)
        ));
        assert!(matches!(
            validate_outbound_url("not a url", UrlKind::Backend),
            Err(UrlValidationError::Parse(_))
        ));
        assert!(matches!(
            validate_outbound_url("file:///etc/passwd", UrlKind::Backend),
            Err(UrlValidationError::BadScheme(_))
        ));
        assert!(matches!(
            validate_outbound_url("javascript:alert(1)", UrlKind::Backend),
            Err(UrlValidationError::BadScheme(_))
        ));
    }

    #[test]
    fn bucket_name_ip_detector_catches_dotted_shapes() {
        for n in [
            "127.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "0177.0.0.1", // octal first octet
            "127.1",      // BSD shorthand
        ] {
            assert!(
                bucket_name_is_ip_like(n),
                "should be detected as IP-like: {n}"
            );
        }
        for n in [
            "my-bucket",
            "builds.deltaglider.io",
            "foo123",
            // Single-token numerics are NOT flagged — they collide
            // with legitimate bucket names like "0400" / "123" / etc.
            // The outbound-URL guard covers the corresponding SSRF.
            "2130706433",
            "0x7f000001",
            "0400",
            "1234567890123456789",
        ] {
            assert!(
                !bucket_name_is_ip_like(n),
                "should NOT be detected as IP-like: {n}"
            );
        }
    }

    #[test]
    fn jwt_alg_allowlist_blocks_none_and_hmac() {
        use jsonwebtoken::Algorithm::*;
        for bad in [HS256, HS384, HS512, EdDSA] {
            assert!(!jwt_alg_is_allowed(bad), "should reject: {bad:?}");
        }
        for ok in [RS256, RS384, RS512, ES256, ES384, PS256, PS384, PS512] {
            assert!(jwt_alg_is_allowed(ok), "should accept: {ok:?}");
        }
    }

    #[test]
    fn public_prefix_validator_enforces_trailing_slash() {
        assert!(validate_public_prefix("").is_ok(), "empty == full bucket");
        assert!(validate_public_prefix("builds/").is_ok());
        assert!(validate_public_prefix("releases/v2/").is_ok());

        assert!(validate_public_prefix("builds").is_err(), "missing slash");
        assert!(validate_public_prefix("../etc").is_err());
        assert!(validate_public_prefix("foo//bar/").is_err());
        assert!(validate_public_prefix("foo\0bar/").is_err());
    }
}
