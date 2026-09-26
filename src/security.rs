// SPDX-License-Identifier: BUSL-1.1

//! Pure security primitives shared across auth and admin surfaces.
//!
//! Everything here is a pure function — no I/O, no global state — so each
//! check has a unit-testable truth table and lives outside the request
//! pipeline. Wire-up happens at the call site.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Constant-time equality for secrets of unequal length.
///
/// `subtle::ConstantTimeEq` on two slices short-circuits on a length
/// mismatch, which leaks the secret's length through timing. Hashing both
/// sides first feeds two fixed `[u8; 32]` arrays into `ct_eq`, so neither
/// length nor prefix is observable. Every secret compare in the crate
/// (bootstrap access key, IAM secret key, metrics bearer token) goes
/// through here.
pub fn secret_eq(a: &[u8], b: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    Sha256::digest(a).ct_eq(&Sha256::digest(b)).into()
}

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
/// [`SsrfGuardedResolver`], which the OIDC + webhook clients install (the S3
/// backend client installs [`SdkSsrfGuardedResolver`]). Callers
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
    // A trailing dot names the same host (`localhost.` == `localhost`).
    let normalised = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();

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
    } else if ips_named_in_hostname(&normalised)
        .into_iter()
        .any(|ip| !ip_is_acceptable(ip, kind))
    {
        // `169.254.169.254.nip.io` resolves to the IP it spells. The
        // resolve-time guard catches it too; refusing it here gives the
        // operator a clear error at config time.
        return Err(UrlValidationError::ForbiddenIp(host.to_string()));
    }

    Ok(())
}

/// Public wildcard-DNS services that answer with the address written in
/// the name, in dotted, dashed or hex form.
const WILDCARD_IP_DNS_SUFFIXES: &[&str] = &["nip.io", "sslip.io", "xip.io", "traefik.me"];

/// Addresses a hostname spells out: any four consecutive decimal labels
/// (`10.0.0.1.example.com`), plus the dashed (`app-10-0-0-1`, IPv6 `--1`)
/// and hex (`0a000001`) label forms under a known wildcard-DNS suffix.
fn ips_named_in_hostname(host: &str) -> Vec<IpAddr> {
    let labels: Vec<&str> = host.split('.').collect();
    let mut out = Vec::new();
    for w in labels.windows(4) {
        if let Ok(ip) = w.join(".").parse::<Ipv4Addr>() {
            out.push(IpAddr::V4(ip));
        }
    }
    let wildcard = WILDCARD_IP_DNS_SUFFIXES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")));
    if wildcard {
        for label in &labels {
            let parts: Vec<&str> = label.split('-').collect();
            if parts.len() >= 4 {
                if let Ok(ip) = parts[parts.len() - 4..].join(".").parse::<Ipv4Addr>() {
                    out.push(IpAddr::V4(ip));
                }
            }
            if let Ok(ip) = label.replace('-', ":").parse::<Ipv6Addr>() {
                out.push(IpAddr::V6(ip));
            }
            if label.len() == 8 {
                if let Ok(n) = u32::from_str_radix(label, 16) {
                    out.push(IpAddr::V4(Ipv4Addr::from(n)));
                }
            }
        }
    }
    out
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

/// The longest prefix of `s` of at most `max_bytes` bytes that ends on a
/// char boundary: THE way to cut an excerpt of client text for a log line.
/// `&s[..n]` panics when byte `n` falls inside a multi-byte character, and a
/// panic in a handler drops the client's connection.
pub fn str_prefix(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

/// AWS IMDS over IPv6 (Nitro instances).
const AWS_IMDS_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);

fn ip_is_metadata_service(ip: IpAddr) -> bool {
    match ip {
        // AWS IMDSv1/IMDSv2, Azure IMDS, GCP metadata server (all same v4).
        IpAddr::V4(v4) => v4.octets() == [169, 254, 169, 254],
        IpAddr::V6(v6) => {
            v6 == AWS_IMDS_V6
                || embedded_ipv4(v6).is_some_and(|m| m.octets() == [169, 254, 169, 254])
        }
    }
}

/// The IPv4 address an IPv6 address carries, for every transition form
/// that routes to it: IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible
/// (`::a.b.c.d`), NAT64 well-known prefix (`64:ff9b::/96`), 6to4
/// (`2002::/16`) and Teredo (`2001::/32`, client address XOR-obfuscated).
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let v4 = |hi: u16, lo: u16| Ipv4Addr::from(((hi as u32) << 16) | lo as u32);
    if let Some(m) = ip.to_ipv4_mapped() {
        return Some(m);
    }
    if s[..6] == [0; 6] || (s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4]) {
        return Some(v4(s[6], s[7]));
    }
    if s[0] == 0x2002 {
        return Some(v4(s[1], s[2]));
    }
    if s[0] == 0x2001 && s[1] == 0 {
        return Some(v4(!s[6], !s[7]));
    }
    None
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
        // 0.0.0.0/8 — "this network"; Linux routes it to the local host
        || o[0] == 0
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
        // NAT64 local-use prefix (RFC 8215) — never globally routed
        || (ip.segments()[0] == 0x64 && ip.segments()[1] == 0xff9b && ip.segments()[2] == 1)
        // Transition forms (mapped, compatible, NAT64, 6to4, Teredo) are as
        // private as the IPv4 address they carry.
        || embedded_ipv4(ip).is_some_and(ipv4_is_private)
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

/// Does the S3 client's resolver guard `name`? It guards the endpoint
/// host and its subdomains (virtual-hosted-style `bucket.host`). Any other
/// name is an HTTP(S) proxy host from the environment: the operator put it
/// there, and it often lives in private space.
pub(crate) fn sdk_resolver_guards_name(endpoint_host: &str, name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == endpoint_host || name.ends_with(&format!(".{endpoint_host}"))
}

/// Is a RESOLVED address for an S3 backend endpoint refused?
///
/// Backend endpoints are admin-configured, and on-prem MinIO/Ceph behind
/// internal DNS (`https://minio.corp` → 10.0.0.5) is a normal deployment. So
/// a resolved name may point at private space; only the cloud-credential
/// targets are refused: link-local (169.254.0.0/16 incl. IMDS, fe80::/10),
/// AWS IPv6 IMDS, and their IPv4-embedded IPv6 forms. `BackendDev` refuses
/// metadata only. Other kinds keep the full [`ip_is_acceptable`] policy.
pub(crate) fn resolved_backend_ip_refused(ip: IpAddr, kind: UrlKind) -> bool {
    match kind {
        UrlKind::Backend => ip_is_metadata_service(ip) || ip_is_link_local(ip),
        UrlKind::BackendDev => ip_is_metadata_service(ip),
        UrlKind::Oidc | UrlKind::Webhook => !ip_is_acceptable(ip, kind),
    }
}

fn ip_is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => {
            (v6.segments()[0] & 0xffc0) == 0xfe80
                || embedded_ipv4(v6).is_some_and(|m| m.is_link_local())
        }
    }
}

fn refused_resolution_message(host: &str, refused: &[IpAddr], kind: UrlKind) -> String {
    let addrs: Vec<String> = refused.iter().map(IpAddr::to_string).collect();
    let fix = match kind {
        UrlKind::Backend | UrlKind::BackendDev => {
            "Cloud-metadata and link-local addresses are never allowed for an S3 endpoint; \
             `allow_local: true` does NOT permit them. Point the endpoint's DNS at the \
             storage server's real address."
        }
        UrlKind::Oidc | UrlKind::Webhook => {
            "Private, loopback and metadata addresses are not allowed for this URL."
        }
    };
    format!(
        "SSRF guard: '{host}' resolved only to refused address(es) [{}]. {fix}",
        addrs.join(", ")
    )
}

/// DNS resolver for the AWS SDK S3 client: the S3 twin of
/// [`SsrfGuardedResolver`]. `validate_outbound_url` checks only the
/// endpoint TEXT, so an endpoint name whose A/AAAA record points at IMDS
/// (or later rebinds there) passed. This resolver drops every address that
/// [`resolved_backend_ip_refused`] refuses for the endpoint host, so the
/// connection fails closed. Literal-IP endpoints never reach a resolver;
/// the text check covers them.
#[derive(Debug, Clone)]
pub struct SdkSsrfGuardedResolver {
    kind: UrlKind,
    /// Lowercase endpoint host, no trailing dot.
    endpoint_host: String,
}

impl SdkSsrfGuardedResolver {
    pub fn new(kind: UrlKind, endpoint_host: &str) -> Self {
        Self {
            kind,
            endpoint_host: endpoint_host.trim_end_matches('.').to_ascii_lowercase(),
        }
    }
}

impl aws_smithy_runtime_api::client::dns::ResolveDns for SdkSsrfGuardedResolver {
    fn resolve_dns<'a>(
        &'a self,
        name: &'a str,
    ) -> aws_smithy_runtime_api::client::dns::DnsFuture<'a> {
        use aws_smithy_runtime_api::client::dns::{DnsFuture, ResolveDnsError};
        DnsFuture::new(async move {
            let ips: Vec<IpAddr> = tokio::net::lookup_host((name, 0))
                .await
                .map_err(ResolveDnsError::new)?
                .map(|sa| sa.ip())
                .collect();
            if !sdk_resolver_guards_name(&self.endpoint_host, name) {
                return Ok(ips);
            }
            let (refused, safe): (Vec<IpAddr>, Vec<IpAddr>) = ips
                .into_iter()
                .partition(|ip| resolved_backend_ip_refused(*ip, self.kind));
            if safe.is_empty() {
                return Err(ResolveDnsError::new(std::io::Error::other(
                    refused_resolution_message(name, &refused, self.kind),
                )));
            }
            Ok(safe)
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn str_prefix_cuts_on_a_char_boundary() {
        assert_eq!(str_prefix("abcdef", 3), "abc");
        assert_eq!(str_prefix("ab", 10), "ab");
        // Byte 2 is inside the first "é" (bytes 1..3).
        assert_eq!(str_prefix("a\u{e9}\u{e9}", 2), "a");
        assert_eq!(str_prefix("\u{4e2d}", 1), "");
    }

    proptest::proptest! {
        #[test]
        fn str_prefix_is_a_bounded_prefix(s in ".{0,16}", n in 0usize..40) {
            let p = str_prefix(&s, n);
            proptest::prop_assert!(p.len() <= n && s.starts_with(p));
            // Maximal: the next char would pass the bound.
            if let Some(c) = s[p.len()..].chars().next() {
                proptest::prop_assert!(p.len() + c.len_utf8() > n);
            }
        }
    }

    use super::*;

    #[test]
    fn secret_eq_ignores_length_and_matches_exactly() {
        assert!(secret_eq(b"abc", b"abc"));
        assert!(!secret_eq(b"abc", b"abd"));
        assert!(!secret_eq(b"abc", b"abcd"), "length differs");
        assert!(!secret_eq(b"", b"a"));
        assert!(secret_eq(b"", b""));
    }

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

    /// S18: forms that reached IMDS / private space past the literal check.
    #[test]
    fn validate_url_blocks_ssrf_bypass_forms() {
        let cases = [
            // Trailing dot: the same host as the dotless name.
            "https://localhost./",
            "https://metadata.google.internal./",
            "https://foo.internal./",
            // Wildcard DNS that answers with the IP written in the name.
            "https://169.254.169.254.nip.io/",
            "https://10.0.0.1.sslip.io/",
            "https://app.127-0-0-1.sslip.io/",
            "https://a9fea9fe.nip.io/",
            // IPv6 forms that carry an IPv4 address.
            "https://[64:ff9b::a9fe:a9fe]/", // NAT64 well-known prefix
            "https://[64:ff9b:1::a9fe:a9fe]/", // NAT64 local-use prefix
            "https://[2002:a9fe:a9fe::]/",   // 6to4
            "https://[::a9fe:a9fe]/",        // IPv4-compatible
            "https://[2002:0a00:0001::]/",   // 6to4 of 10.0.0.1
            // AWS IMDS over IPv6.
            "https://[fd00:ec2::254]/",
            // 0.0.0.0/8 "this network".
            "https://0.1.2.3/",
        ];
        for u in cases {
            assert!(
                validate_outbound_url(u, UrlKind::Backend).is_err(),
                "should reject: {u}"
            );
        }
        // Metadata stays blocked even in dev, in every carrier form.
        for u in [
            "http://[fd00:ec2::254]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[2002:a9fe:a9fe::]/",
            "http://[::a9fe:a9fe]/",
            "http://169.254.169.254.nip.io/",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::BackendDev).is_err(),
                "dev must still reject metadata: {u}"
            );
        }
        // Dev keeps its local targets.
        assert!(validate_outbound_url("http://localhost.:9000/", UrlKind::BackendDev).is_ok());
        // Public names with digits stay usable.
        for u in [
            "https://s3.eu-central-1.amazonaws.com/",
            "https://fsn1.your-objectstorage.com/",
            "https://s3.us-west-000.backblazeb2.com/",
            "https://8.8.8.8.example.com/",
            "https://[2001:db9::1]/",
        ] {
            assert!(
                validate_outbound_url(u, UrlKind::Backend).is_ok(),
                "should accept: {u}"
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

    #[test]
    fn resolved_backend_ip_policy_table() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // (address, refused for Backend, refused for BackendDev)
        for (a, backend, dev) in [
            ("169.254.169.254", true, true),
            ("169.254.10.20", true, false),
            ("fe80::1", true, false),
            ("fd00:ec2::254", true, true),
            ("64:ff9b::a9fe:a9fe", true, true),
            ("2002:a9fe:a9fe::", true, true),
            ("::a9fe:a9fe", true, true),
            ("::ffff:169.254.169.254", true, true),
            ("64:ff9b::a9fe:0101", true, false),
            ("10.0.0.5", false, false),
            ("172.16.0.1", false, false),
            ("127.0.0.1", false, false),
            ("::1", false, false),
            ("fd12::5", false, false),
            ("8.8.8.8", false, false),
        ] {
            assert_eq!(
                resolved_backend_ip_refused(ip(a), UrlKind::Backend),
                backend,
                "{a}"
            );
            assert_eq!(
                resolved_backend_ip_refused(ip(a), UrlKind::BackendDev),
                dev,
                "{a}"
            );
        }
        // OIDC/webhook keep the strict policy.
        assert!(resolved_backend_ip_refused(ip("10.0.0.5"), UrlKind::Oidc));
        assert!(resolved_backend_ip_refused(
            ip("127.0.0.1"),
            UrlKind::Webhook
        ));
    }

    #[test]
    fn sdk_resolver_guards_endpoint_host_and_subdomains_only() {
        assert!(sdk_resolver_guards_name("s3.example.com", "s3.example.com"));
        assert!(sdk_resolver_guards_name(
            "s3.example.com",
            "S3.Example.com."
        ));
        assert!(sdk_resolver_guards_name(
            "s3.example.com",
            "releases.s3.example.com"
        ));
        // A proxy host (or a look-alike) is not the endpoint.
        assert!(!sdk_resolver_guards_name("s3.example.com", "proxy.corp"));
        assert!(!sdk_resolver_guards_name(
            "s3.example.com",
            "evils3.example.com"
        ));
    }

    #[tokio::test]
    async fn sdk_resolver_rejects_forbidden_addresses_for_the_endpoint() {
        use aws_smithy_runtime_api::client::dns::ResolveDns;
        // A literal resolves to itself via lookup_host: no real DNS needed.
        let strict = SdkSsrfGuardedResolver::new(UrlKind::Backend, "169.254.169.254");
        let err = strict.resolve_dns("169.254.169.254").await.unwrap_err();
        let msg = std::error::Error::source(&err).unwrap().to_string();
        assert!(msg.contains("'169.254.169.254'"), "{msg}");
        assert!(msg.contains("[169.254.169.254]"), "{msg}");
        assert!(msg.contains("allow_local: true` does NOT permit"), "{msg}");
        // On-prem storage behind internal DNS: a private answer is fine.
        for ip in ["10.0.0.5", "192.168.1.9", "127.0.0.1", "fd12::5"] {
            let r = SdkSsrfGuardedResolver::new(UrlKind::Backend, ip);
            assert!(r.resolve_dns(ip).await.is_ok(), "private answer {ip}");
        }
        for ip in ["169.254.1.1", "fe80::1", "fd00:ec2::254"] {
            let r = SdkSsrfGuardedResolver::new(UrlKind::Backend, ip);
            assert!(r.resolve_dns(ip).await.is_err(), "metadata/link-local {ip}");
        }
        // Dev allows private space but never metadata.
        let dev = SdkSsrfGuardedResolver::new(UrlKind::BackendDev, "10.0.0.1");
        assert!(dev.resolve_dns("10.0.0.1").await.is_ok());
        let dev = SdkSsrfGuardedResolver::new(UrlKind::BackendDev, "169.254.169.254");
        assert!(dev.resolve_dns("169.254.169.254").await.is_err());
        // A non-endpoint name (an env proxy) passes through unfiltered.
        let strict = SdkSsrfGuardedResolver::new(UrlKind::Backend, "s3.example.com");
        assert!(strict.resolve_dns("10.0.0.1").await.is_ok());
    }
}

#[cfg(test)]
mod outbound_url_proptests {
    use super::{validate_outbound_url, UrlKind};
    use proptest::prelude::*;
    use std::net::Ipv4Addr;

    fn forbidden_v4() -> impl Strategy<Value = Ipv4Addr> {
        prop_oneof![
            Just(Ipv4Addr::new(169, 254, 169, 254)),
            any::<[u8; 3]>().prop_map(|o| Ipv4Addr::new(10, o[0], o[1], o[2])),
            any::<[u8; 3]>().prop_map(|o| Ipv4Addr::new(127, o[0], o[1], o[2])),
            any::<[u8; 2]>().prop_map(|o| Ipv4Addr::new(192, 168, o[0], o[1])),
        ]
    }

    proptest! {
        /// A forbidden IPv4 address stays forbidden in every carrier form:
        /// literal, trailing dot, wildcard DNS name, and each IPv6 embedding.
        #[test]
        fn forbidden_v4_rejected_in_every_carrier(ip in forbidden_v4(), sub in "[a-z]{1,8}") {
            let [a, b, c, d] = ip.octets();
            let hex = u32::from(ip);
            let (hi, lo) = (hex >> 16, hex & 0xffff);
            let urls = [
                format!("https://{ip}/"),
                format!("https://{ip}./"),
                format!("https://{ip}.nip.io/"),
                format!("https://{sub}.{ip}.sslip.io/"),
                format!("https://{sub}-{a}-{b}-{c}-{d}.sslip.io/"),
                format!("https://{hex:08x}.nip.io/"),
                format!("https://[::ffff:{ip}]/"),
                format!("https://[64:ff9b::{hi:x}:{lo:x}]/"),
                format!("https://[2002:{hi:x}:{lo:x}::1]/"),
                format!("https://[::{hi:x}:{lo:x}]/"),
            ];
            for u in urls {
                prop_assert!(validate_outbound_url(&u, UrlKind::Backend).is_err(), "accepted {}", u);
            }
        }

        /// The validator never panics.
        #[test]
        fn outbound_never_panics(s in ".{0,120}") {
            let _ = validate_outbound_url(&s, UrlKind::Backend);
            let _ = validate_outbound_url(&format!("https://{s}/"), UrlKind::BackendDev);
        }
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
