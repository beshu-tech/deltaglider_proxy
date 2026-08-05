// SPDX-License-Identifier: BUSL-1.1

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
/// pointing `legit.example.com` at `169.254.169.254` would still pass this
/// cheap first-line check — that DNS-rebinding gap is closed at connect time by
/// [`SsrfGuardedResolver`], which the OIDC + webhook clients install. Callers
/// pair this with `redirect(Policy::none())` and the guarded resolver.
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

    let name_hit = FORBIDDEN_HOSTNAMES.iter().any(|h| normalised == *h);
    let suffix_hit = FORBIDDEN_SUFFIXES.iter().any(|s| normalised.ends_with(s));
    if name_hit || suffix_hit {
        // BackendDev permits `localhost` and forbidden SUFFIXES (k8s in-cluster DNS
        // ends `.svc.cluster.local`); named metadata hosts stay blocked even in dev.
        let dev_ok = matches!(kind, UrlKind::BackendDev)
            && (!name_hit || DEV_ALLOWED.iter().any(|h| normalised == *h));
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

/// Why a bucket name was rejected. Carries enough context for each call
/// site to render its own error (the S3 API extractor maps these to
/// `S3Error::InvalidBucketName` strings; the CLU collapses to a `bool`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketNameError {
    /// Name is empty.
    Empty,
    /// Name is shorter than 3 or longer than 63 characters.
    BadLength(usize),
    /// Name contains a character outside `[a-z0-9.-]`.
    BadChar,
    /// Name contains consecutive dots (`..`) — also a path-traversal vector.
    ConsecutiveDots,
    /// Name does not start with a lowercase letter or digit.
    BadStart,
    /// Name does not end with a lowercase letter or digit.
    BadEnd,
    /// Name parses as an IP literal in some dotted notation (S3 forbids these).
    IpLike,
}

impl std::fmt::Display for BucketNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BucketNameError::Empty => write!(f, "Bucket name cannot be empty"),
            BucketNameError::BadLength(len) => write!(
                f,
                "Bucket name must be between 3 and 63 characters long, got {len}"
            ),
            BucketNameError::BadChar => write!(
                f,
                "Bucket name can only contain lowercase letters, numbers, hyphens, and dots"
            ),
            BucketNameError::ConsecutiveDots => {
                write!(f, "Bucket name must not contain consecutive dots")
            }
            BucketNameError::BadStart => {
                write!(f, "Bucket name must start with a letter or number")
            }
            BucketNameError::BadEnd => write!(f, "Bucket name must end with a letter or number"),
            BucketNameError::IpLike => {
                write!(f, "Bucket name must not be formatted as an IP address")
            }
        }
    }
}

/// Canonical S3 bucket-name validator. The CLI URL parser
/// (`cli/s3_url.rs`) collapses to `validate_bucket_name(name).is_ok()`;
/// the live S3 request path delegates bucket-name syntax to the upstream
/// `s3s` framework, so this is the single in-crate source of the rule.
///
/// Enforces the S3 DNS-compatible naming rules — 3-63 chars, lowercase
/// ASCII + digits + `.` + `-`, start/end alphanumeric, no `..` — AND
/// rejects IP-shaped names via [`bucket_name_is_ip_like`]. The IP-rejection
/// is security-relevant: an IP-shaped bucket name on the filesystem backend
/// is harmless, but it breaks downstream SSRF heuristics that key off
/// "does this look like an IP", so we forbid it (intentionally stricter than
/// AWS, which only forbids the literal 4-octet dotted-quad — we also reject
/// BSD-shorthand and radix-tagged forms like `127.1` / `0x7f.0.0.1`).
///
/// Pure: no I/O. Order of checks is fixed so the returned variant is
/// deterministic for a given input (tested below).
pub fn validate_bucket_name(name: &str) -> Result<(), BucketNameError> {
    if name.is_empty() {
        return Err(BucketNameError::Empty);
    }
    let len = name.len();
    if !(3..=63).contains(&len) {
        return Err(BucketNameError::BadLength(len));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(BucketNameError::BadChar);
    }
    if name.contains("..") {
        return Err(BucketNameError::ConsecutiveDots);
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(BucketNameError::BadStart);
    }
    if !name.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(BucketNameError::BadEnd);
    }
    if bucket_name_is_ip_like(name) {
        return Err(BucketNameError::IpLike);
    }
    Ok(())
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

pub(crate) fn ip_is_acceptable(ip: IpAddr, kind: UrlKind) -> bool {
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

/// reqwest DNS resolver that closes the DNS-rebinding gap `validate_outbound_url`
/// documents: it resolves the hostname via the system resolver, then drops any
/// address that fails [`ip_is_acceptable`] for this [`UrlKind`]. A hostile A
/// record pointing a legit name at 169.254.169.254 / private space yields zero
/// acceptable addresses → the connection fails closed. Attach via
/// `reqwest::Client::builder().dns_resolver(Arc::new(SsrfGuardedResolver::new(kind)))`.
pub struct SsrfGuardedResolver {
    kind: UrlKind,
}

impl SsrfGuardedResolver {
    pub fn new(kind: UrlKind) -> Self {
        Self { kind }
    }
}

impl reqwest::dns::Resolve for SsrfGuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let kind = self.kind;
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port is irrelevant to the policy; lookup_host needs one. reqwest
            // overrides it with the URL's actual port afterward.
            let addrs = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
            let safe: Vec<std::net::SocketAddr> =
                addrs.filter(|sa| ip_is_acceptable(sa.ip(), kind)).collect();
            if safe.is_empty() {
                return Err(format!(
                    "SSRF guard: '{host}' resolved only to forbidden addresses (metadata/private)"
                )
                .into());
            }
            let iter: reqwest::dns::Addrs = Box::new(safe.into_iter());
            Ok(iter)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_dev_allows_kubernetes_cluster_dns_but_never_metadata() {
        // In-cluster service DNS ends `.svc.cluster.local` — a standard k8s backend
        // shape (caught live: MinIO-in-cluster was unreachable even with allow_local).
        let k8s = "http://minio.dgp.svc.cluster.local:9000";
        assert!(validate_outbound_url(k8s, UrlKind::BackendDev).is_ok());
        // Production mode still refuses it.
        assert!(validate_outbound_url(k8s, UrlKind::Backend).is_err());
        // Named metadata hosts stay blocked even in dev, suffix or not.
        for u in [
            "http://metadata.google.internal/",
            "http://metadata.aws/",
            "http://metadata/",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::BackendDev).is_err(),
                "dev must still reject {u}"
            );
        }
        // Other forbidden suffixes open up in dev only.
        assert!(
            validate_outbound_url("http://ceph.storage.internal:7480", UrlKind::BackendDev).is_ok()
        );
        assert!(
            validate_outbound_url("http://ceph.storage.internal:7480", UrlKind::Backend).is_err()
        );
    }

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

    #[test]
    fn validate_bucket_name_accepts_legal_names() {
        for n in [
            "my-bucket",
            "abc",
            "a.b.c",
            "releases-2024",
            "0400", // numeric but single-token → not IP-like
            "x".repeat(63).as_str(),
        ] {
            assert!(
                validate_bucket_name(n).is_ok(),
                "{n} should be a valid bucket name"
            );
        }
    }

    #[test]
    fn validate_bucket_name_rejects_with_specific_reasons() {
        use BucketNameError::*;
        assert_eq!(validate_bucket_name(""), Err(Empty));
        assert_eq!(validate_bucket_name("ab"), Err(BadLength(2)));
        assert_eq!(validate_bucket_name(&"x".repeat(64)), Err(BadLength(64)));
        assert_eq!(validate_bucket_name("My-Bucket"), Err(BadChar)); // uppercase
        assert_eq!(validate_bucket_name("a_b_c"), Err(BadChar)); // underscore
        assert_eq!(validate_bucket_name("a..b"), Err(ConsecutiveDots));
        assert_eq!(validate_bucket_name("-abc"), Err(BadStart));
        assert_eq!(validate_bucket_name(".abc"), Err(BadStart));
        assert_eq!(validate_bucket_name("abc-"), Err(BadEnd));
        assert_eq!(validate_bucket_name("abc."), Err(BadEnd));
    }

    /// The behaviour change B3 was about: the S3 API path (which previously
    /// used a hand-rolled validator) now rejects IP-shaped names too.
    #[test]
    fn validate_bucket_name_rejects_ip_like() {
        use BucketNameError::*;
        assert_eq!(validate_bucket_name("127.0.0.1"), Err(IpLike));
        assert_eq!(validate_bucket_name("10.0.0.1"), Err(IpLike));
        // BSD shorthand and radix-tagged forms covered by bucket_name_is_ip_like.
        assert_eq!(validate_bucket_name("127.1"), Err(IpLike));
    }

    // SsrfGuardedResolver: a literal IP used as a "host" resolves to itself via
    // lookup_host, so we can exercise the accept/reject filter without real DNS.
    #[tokio::test]
    async fn ssrf_resolver_rejects_metadata_and_private_addresses() {
        use reqwest::dns::Resolve;
        let resolver = SsrfGuardedResolver::new(UrlKind::Oidc);
        let resolve = |h: &str| {
            let name: reqwest::dns::Name = h.parse().unwrap();
            resolver.resolve(name)
        };
        // Cloud metadata + private ranges → empty after filter → error.
        assert!(
            resolve("169.254.169.254").await.is_err(),
            "IMDS must be rejected"
        );
        assert!(
            resolve("10.0.0.1").await.is_err(),
            "private must be rejected"
        );
        assert!(
            resolve("127.0.0.1").await.is_err(),
            "loopback must be rejected"
        );
        // A public literal → at least one acceptable addr → Ok.
        let ok = resolve("8.8.8.8").await.expect("public addr accepted");
        assert!(ok.count() >= 1);
    }
}

#[cfg(test)]
mod bucket_name_proptests {
    use super::{validate_bucket_name, BucketNameError};
    use proptest::prelude::*;

    proptest! {
        /// Any name `validate_bucket_name` accepts must satisfy every
        /// individual S3 rule — this is the invariant a reader relies on.
        #[test]
        fn accepted_names_satisfy_all_rules(name in ".{0,80}") {
            if validate_bucket_name(&name).is_ok() {
                prop_assert!((3..=63).contains(&name.len()));
                prop_assert!(name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.'));
                prop_assert!(!name.contains(".."));
                prop_assert!(name.starts_with(|c: char| c.is_ascii_alphanumeric()));
                prop_assert!(name.ends_with(|c: char| c.is_ascii_alphanumeric()));
                prop_assert!(!super::bucket_name_is_ip_like(&name));
            }
        }

        /// The validator never panics and always returns a determinate result.
        #[test]
        fn never_panics(name in ".{0,200}") {
            let _: Result<(), BucketNameError> = validate_bucket_name(&name);
        }
    }
}
