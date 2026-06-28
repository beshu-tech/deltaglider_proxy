// SPDX-License-Identifier: GPL-3.0-only

//! Per-IP rate limiter for authentication endpoints.
//!
//! Uses a token bucket approach: each IP gets `max_attempts` attempts within a
//! rolling `window`. After exhausting attempts, the IP is locked out for `lockout`
//! duration. Expired entries are periodically cleaned up.

use dashmap::DashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-IP rate limiter for brute-force protection.
///
/// Additionally carries a per-**account** (subject) bucket so password
/// endpoints can be defended against distributed-IP brute force — an
/// attacker rotating IPs across a /16 botnet can chew through the
/// per-IP budget freely, but the per-account bucket caps total
/// attempts against a specific bootstrap password / AKID regardless
/// of the source IP.
///
/// The two buckets have INDEPENDENT policies because the threat
/// models are different:
/// - per-IP: generous, catches single-host noise
/// - per-account: tight, catches distributed credential stuffing
#[derive(Clone)]
pub struct RateLimiter {
    /// Map from IP to (failure_count, first_failure_time, lockout_start).
    entries: Arc<DashMap<IpAddr, RateLimitEntry>>,
    /// Per-account/subject bucket. Keys are caller-supplied strings:
    /// the bootstrap password endpoint uses `"bootstrap"`; `login_as`
    /// uses the access-key-id. Empty string means "no account
    /// dimension applicable" — the per-account check short-circuits
    /// to allow.
    account_entries: Arc<DashMap<String, RateLimitEntry>>,
    /// Maximum failed attempts before lockout (per-IP).
    max_attempts: u32,
    /// Rolling window for counting attempts (per-IP).
    window: Duration,
    /// Lockout duration after max_attempts exceeded (per-IP).
    lockout: Duration,
    /// Per-account policy. Same shape as the per-IP triple.
    account_max_attempts: u32,
    account_window: Duration,
    account_lockout: Duration,
}

struct RateLimitEntry {
    /// Number of failed attempts in the current window.
    count: u32,
    /// When the first failure in the current window occurred.
    window_start: Instant,
    /// When lockout was triggered (None if not locked out).
    lockout_start: Option<Instant>,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// - `max_attempts`: max failures before lockout (per-IP)
    /// - `window`: time window for counting failures (per-IP)
    /// - `lockout`: lockout duration after exceeding max_attempts (per-IP)
    ///
    /// The per-account policy defaults to a STRICTER profile
    /// (10 attempts / 1h window / 1h lockout). Use
    /// `with_account_policy` to override.
    ///
    /// See `default_auth()` for production env-driven defaults.
    pub fn new(max_attempts: u32, window: Duration, lockout: Duration) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            account_entries: Arc::new(DashMap::new()),
            max_attempts,
            window,
            lockout,
            account_max_attempts: 10,
            account_window: Duration::from_secs(3600),
            account_lockout: Duration::from_secs(3600),
        }
    }

    /// Override the per-account policy.
    pub fn with_account_policy(
        mut self,
        max_attempts: u32,
        window: Duration,
        lockout: Duration,
    ) -> Self {
        self.account_max_attempts = max_attempts;
        self.account_window = window;
        self.account_lockout = lockout;
        self
    }

    /// Create a rate limiter from environment variables with defaults:
    /// - `DGP_RATE_LIMIT_MAX_ATTEMPTS`: max failures before lockout (default: 100, per-IP)
    /// - `DGP_RATE_LIMIT_WINDOW_SECS`: rolling window in seconds (default: 300 = 5 min, per-IP)
    /// - `DGP_RATE_LIMIT_LOCKOUT_SECS`: lockout duration in seconds (default: 600 = 10 min, per-IP)
    /// - `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS`: per-account cap (default: 10)
    /// - `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS`: per-account window (default: 3600 = 1h)
    /// - `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS`: per-account lockout (default: 3600 = 1h)
    ///
    /// Default per-account 10/1h/1h is tight on purpose: an attacker
    /// rotating IPs across a botnet shouldn't be able to chew through
    /// more than 10 password guesses per hour against any single
    /// account.
    pub fn default_auth() -> Self {
        use crate::config::env_parse_with_default;
        let max_attempts: u32 = env_parse_with_default("DGP_RATE_LIMIT_MAX_ATTEMPTS", 100);
        let window_secs: u64 = env_parse_with_default("DGP_RATE_LIMIT_WINDOW_SECS", 300);
        let lockout_secs: u64 = env_parse_with_default("DGP_RATE_LIMIT_LOCKOUT_SECS", 600);
        let acct_max: u32 = env_parse_with_default("DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS", 10);
        let acct_win: u64 = env_parse_with_default("DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS", 3600);
        let acct_lock: u64 = env_parse_with_default("DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS", 3600);
        tracing::info!(
            "Rate limiter: per-IP {}/{}s/{}s lockout, per-account {}/{}s/{}s lockout",
            max_attempts,
            window_secs,
            lockout_secs,
            acct_max,
            acct_win,
            acct_lock
        );
        Self::new(
            max_attempts,
            Duration::from_secs(window_secs),
            Duration::from_secs(lockout_secs),
        )
        .with_account_policy(
            acct_max,
            Duration::from_secs(acct_win),
            Duration::from_secs(acct_lock),
        )
    }

    /// Check if an IP is currently rate-limited.
    /// Returns `true` if the request should be BLOCKED.
    pub fn is_limited(&self, ip: &IpAddr) -> bool {
        let entry = match self.entries.get(ip) {
            Some(e) => e,
            None => return false,
        };

        let now = Instant::now();

        // Check lockout
        if let Some(lockout_start) = entry.lockout_start {
            if now.duration_since(lockout_start) < self.lockout {
                return true; // Still locked out
            }
            // Lockout expired — will be cleaned up or reset on next record_failure
        }

        false
    }

    /// Get the progressive delay for an IP based on failure count.
    /// Returns a duration to sleep before responding (makes brute force expensive).
    /// No delay for the first 10 failures (normal typos/misconfiguration).
    /// After that, doubles each time from 200ms: 200ms (11), 400ms (12),
    /// 800ms (13), 1.6s (14), 3.2s (15), then capped at 5s (16+).
    /// Capped at 5 seconds to avoid tying up connections forever.
    pub fn progressive_delay(&self, ip: &IpAddr) -> Duration {
        let entry = match self.entries.get(ip) {
            Some(e) => e,
            None => return Duration::ZERO,
        };
        if entry.count <= 10 {
            return Duration::ZERO;
        }
        let excess = entry.count - 10;
        let delay_ms = 100u64.saturating_mul(1u64 << excess.min(6));
        Duration::from_millis(delay_ms.min(5000))
    }

    /// Get the current failure count for an IP (for logging).
    pub fn failure_count(&self, ip: &IpAddr) -> u32 {
        self.entries.get(ip).map(|e| e.count).unwrap_or(0)
    }

    /// Record a failed authentication attempt for an IP.
    /// Returns `true` if the IP is now rate-limited (should block further attempts).
    pub fn record_failure(&self, ip: &IpAddr) -> bool {
        let now = Instant::now();

        let mut entry = self.entries.entry(*ip).or_insert(RateLimitEntry {
            count: 0,
            window_start: now,
            lockout_start: None,
        });

        // If lockout has expired, reset the entry
        if let Some(lockout_start) = entry.lockout_start {
            if now.duration_since(lockout_start) >= self.lockout {
                entry.count = 0;
                entry.window_start = now;
                entry.lockout_start = None;
            } else {
                return true; // Still locked out
            }
        }

        // If window has expired, reset the counter
        if now.duration_since(entry.window_start) >= self.window {
            entry.count = 0;
            entry.window_start = now;
        }

        entry.count += 1;

        if entry.count >= self.max_attempts {
            entry.lockout_start = Some(now);
            true
        } else {
            false
        }
    }

    /// Record a successful authentication (resets the failure counter for the IP).
    pub fn record_success(&self, ip: &IpAddr) {
        self.entries.remove(ip);
    }

    /// Per-account variant: is this subject (bootstrap / AKID /
    /// username) currently locked out? Empty subject → not limited
    /// (caller didn't supply a subject dimension).
    pub fn is_limited_account(&self, subject: &str) -> bool {
        if subject.is_empty() {
            return false;
        }
        let Some(entry) = self.account_entries.get(subject) else {
            return false;
        };
        let now = Instant::now();
        if let Some(lockout_start) = entry.lockout_start {
            if now.duration_since(lockout_start) < self.account_lockout {
                return true;
            }
        }
        false
    }

    /// Per-account variant: record a failed attempt for this subject.
    /// Mirrors `record_failure` semantics for the per-account bucket.
    pub fn record_failure_account(&self, subject: &str) -> bool {
        if subject.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut entry = self
            .account_entries
            .entry(subject.to_string())
            .or_insert(RateLimitEntry {
                count: 0,
                window_start: now,
                lockout_start: None,
            });

        if let Some(lockout_start) = entry.lockout_start {
            if now.duration_since(lockout_start) >= self.account_lockout {
                entry.count = 0;
                entry.window_start = now;
                entry.lockout_start = None;
            } else {
                return true;
            }
        }
        if now.duration_since(entry.window_start) >= self.account_window {
            entry.count = 0;
            entry.window_start = now;
        }
        entry.count += 1;
        if entry.count >= self.account_max_attempts {
            entry.lockout_start = Some(now);
            true
        } else {
            false
        }
    }

    /// Per-account variant: clear the failure counter for this subject.
    pub fn record_success_account(&self, subject: &str) {
        if subject.is_empty() {
            return;
        }
        self.account_entries.remove(subject);
    }

    /// Remove expired entries to prevent unbounded memory growth.
    /// Call this periodically (e.g., every 5 minutes).
    pub fn cleanup_expired(&self) {
        let now = Instant::now();

        self.entries
            .retain(|_ip, entry| should_keep_entry(entry, now, self.lockout, self.window));

        self.account_entries.retain(|_subj, entry| {
            should_keep_entry(entry, now, self.account_lockout, self.account_window)
        });
    }
}

/// Whether a rate-limit entry should be retained during cleanup.
///
/// Keep it if it is currently locked out (and the lockout hasn't expired),
/// or if it is still within the active counting window. Shared by the
/// per-IP and per-account retention passes, which differ only in policy.
fn should_keep_entry(
    entry: &RateLimitEntry,
    now: Instant,
    lockout: Duration,
    window: Duration,
) -> bool {
    // Keep entries that are currently locked out and lockout hasn't expired
    if let Some(lockout_start) = entry.lockout_start {
        if now.duration_since(lockout_start) < lockout {
            return true; // Keep: still locked out
        }
    }
    // Keep entries within the active window
    now.duration_since(entry.window_start) < window
}

/// Whether proxy-set headers (X-Forwarded-For, X-Real-IP) should be trusted
/// for client IP extraction. When `false`, these headers are ignored to prevent
/// IP spoofing by untrusted clients.
///
/// Controlled by `DGP_TRUST_PROXY_HEADERS`. **Defaults to `false`** for
/// secure-by-default behaviour: direct-to-internet deployments are protected
/// against IP spoofing out of the box.
///
/// Deployments behind a trusted reverse proxy (nginx, Caddy, ALB) should set
/// `DGP_TRUST_PROXY_HEADERS=true` so the proxy can extract the real client IP
/// from `X-Forwarded-For` / `X-Real-IP` headers for rate limiting and
/// `aws:SourceIp` IAM conditions. The connection peer IP is always available
/// via `ConnectInfo<SocketAddr>` (see `extract_client_ip_with_peer`), so per-IP
/// rate limiting works regardless of this setting.
pub fn trust_proxy_headers() -> bool {
    crate::config::env_bool("DGP_TRUST_PROXY_HEADERS", false)
}

/// Extract client IP from request headers/connection info.
///
/// When `DGP_TRUST_PROXY_HEADERS=true`, checks X-Forwarded-For and X-Real-IP
/// (for deployments behind a trusted reverse proxy). Otherwise ignores these
/// headers to prevent IP spoofing.
///
/// Returns `None` if no IP can be determined. In this case, rate limiting is
/// skipped for this request (the SigV4 signature check still applies). Callers
/// with the connection peer IP should use `extract_client_ip_with_peer`.
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> Option<IpAddr> {
    extract_client_ip_with_peer(headers, None)
}

/// Extract client IP from trusted proxy headers or peer socket fallback.
///
/// Resolution order:
/// 1. Trusted proxy headers (`X-Forwarded-For`, `X-Real-IP`) when
///    `DGP_TRUST_PROXY_HEADERS=true`.
/// 2. Direct peer socket IP from axum `ConnectInfo` (when provided).
/// 3. `None` if neither source is available.
pub fn extract_client_ip_with_peer(
    headers: &axum::http::HeaderMap,
    peer_ip: Option<IpAddr>,
) -> Option<IpAddr> {
    if trust_proxy_headers() {
        // Check X-Forwarded-For header (first IP is the client)
        if let Some(xff) = headers.get("x-forwarded-for") {
            if let Ok(xff_str) = xff.to_str() {
                if let Some(first_ip) = xff_str.split(',').next() {
                    if let Ok(ip) = first_ip.trim().parse::<IpAddr>() {
                        return Some(normalize_ip(ip));
                    }
                }
            }
        }

        // Check X-Real-IP header
        if let Some(real_ip) = headers.get("x-real-ip") {
            if let Ok(ip_str) = real_ip.to_str() {
                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                    return Some(normalize_ip(ip));
                }
            }
        }
    }

    peer_ip.map(normalize_ip)
}

/// Collapse IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) back to their
/// plain IPv4 form before the rate limiter keys on them.
///
/// Without this step, an attacker can double their brute-force budget by
/// alternating `1.2.3.4` and `::ffff:1.2.3.4` — both are the same host to
/// the TCP stack but `IpAddr::V4` and `IpAddr::V6` hash to different
/// buckets in the DashMap. Normalising to V4 closes the bypass.
///
/// Non-mapped IPv6 addresses pass through unchanged.
pub(crate) fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

// ════════════════════════════════════════════════════════════════════════
// Guard — ergonomic rate-limit wrapper for auth endpoints
// ════════════════════════════════════════════════════════════════════════
//
// This guard collapses the ~10-line "extract IP → is_limited? → progressive
// delay → record failure/success" pattern that appeared at five+ endpoints
// into a two-call shape:
//
// ```ignore
// let guard = match RateLimitGuard::enter(&state.rate_limiter, &headers, "admin").await {
//     Ok(g) => g,
//     Err(blocked) => return /* handler-specific 429 response */,
// };
// // ... do the auth check ...
// if bad { guard.record_failure(); return 401; }
// guard.record_success();
// ```
//
// Why a guard and not free functions:
// - The "log on lockout transition" logic needs both `record_failure`'s
//   return value AND the `event_prefix` string; bundling them onto the
//   guard avoids passing both at every call site.
// - `record_failure` vs `record_success` are clearly adjacent operations
//   that share the same `(rl, ip)` context; a method call on a guard reads
//   more naturally than `rate_limit::record_failure(rl, ip, "admin")`.
// - The unspecified-IP fallback (present at three+ sites before the guard)
//   is encapsulated once, inside `enter`.
//
// Callers still own response construction — the guard makes NO assumption
// about the response type because endpoints use varying shapes
// (`Json<LoginResponse>`, `Result<_, StatusCode>`, structured admin JSON).

/// Signals that `RateLimitGuard::enter` short-circuited because the caller's
/// IP is currently locked out. The handler is expected to turn this into
/// its own 429 response immediately — no further operations should be
/// attempted under rate-limit protection.
///
/// The `ip` and `failure_count` fields are diagnostic — they're
/// populated for `Debug` output / future telemetry but no caller
/// destructures them today, so `dead_code` is allowed explicitly.
/// Callers should treat any `Err(_)` from the guard as "blocked".
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct Blocked {
    pub(crate) ip: IpAddr,
    pub(crate) failure_count: u32,
}

/// RAII-style wrapper that ties a rate-limited operation to the
/// `(RateLimiter, IpAddr, event_prefix)` triple it needs. The guard
/// itself does not enforce cleanup at drop — callers must explicitly
/// call `record_success` or `record_failure` to communicate the outcome.
/// Dropping without calling either is valid and means "no-op" (useful
/// for short-circuits that aren't auth failures, e.g. internal errors).
pub struct RateLimitGuard<'a> {
    rl: &'a RateLimiter,
    ip: IpAddr,
    /// The account-dimension key — empty when the caller didn't
    /// supply one. Currently set by callers via `enter_with_account`.
    subject: String,
    event_prefix: &'static str,
}

impl<'a> RateLimitGuard<'a> {
    /// Begin rate-limit-protected execution (per-IP only).
    /// See `enter_with_account` for endpoints that also need a
    /// per-account bucket (the password endpoint, `login_as`).
    pub async fn enter(
        rl: &'a RateLimiter,
        headers: &axum::http::HeaderMap,
        peer_ip: Option<IpAddr>,
        event_prefix: &'static str,
    ) -> Result<Self, Blocked> {
        Self::enter_with_account(rl, headers, peer_ip, "", event_prefix).await
    }

    /// Like [`enter`], but also consults the per-account bucket. If
    /// EITHER the per-IP or per-account bucket reports locked, the
    /// guard returns `Err(Blocked)`. `subject` is the account key —
    /// `"bootstrap"` for the bootstrap password, the access-key-id
    /// for `login_as`, etc. Empty string degrades to per-IP only.
    pub async fn enter_with_account(
        rl: &'a RateLimiter,
        headers: &axum::http::HeaderMap,
        peer_ip: Option<IpAddr>,
        subject: &str,
        event_prefix: &'static str,
    ) -> Result<Self, Blocked> {
        let ip = extract_client_ip_with_peer(headers, peer_ip)
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        if rl.is_limited(&ip) {
            let failure_count = rl.failure_count(&ip);
            tracing::warn!(
                "SECURITY | event={}_brute_force_blocked | scope=ip | ip={} | attempts={}",
                event_prefix,
                ip,
                failure_count
            );
            return Err(Blocked { ip, failure_count });
        }
        if !subject.is_empty() && rl.is_limited_account(subject) {
            tracing::warn!(
                "SECURITY | event={}_brute_force_blocked | scope=account | subject={} | ip={}",
                event_prefix,
                sanitize_for_log(subject),
                ip
            );
            return Err(Blocked {
                ip,
                failure_count: 0,
            });
        }
        let delay = rl.progressive_delay(&ip);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(Self {
            rl,
            ip,
            subject: subject.to_string(),
            event_prefix,
        })
    }

    /// The IP the guard is scoped to. Exposed for callers that need to
    /// include the IP in their response body or own audit logs.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// Record a successful operation. Resets the failure counter for
    /// BOTH the per-IP and per-account buckets so future attempts
    /// start from zero on either dimension.
    pub fn record_success(&self) {
        self.rl.record_success(&self.ip);
        self.rl.record_success_account(&self.subject);
    }

    /// Record a failed operation. Increments BOTH bucket counters
    /// (per-IP and per-account, when a subject is set). On lockout
    /// transition emits a SECURITY log event tagged with
    /// `event_prefix` and which dimension tripped.
    pub fn record_failure(&self) {
        let ip_locked = self.rl.record_failure(&self.ip);
        if ip_locked {
            let count = self.rl.failure_count(&self.ip);
            tracing::warn!(
                "SECURITY | event={}_brute_force_lockout | scope=ip | ip={} | attempts={}",
                self.event_prefix,
                self.ip,
                count
            );
        }
        if !self.subject.is_empty() {
            let acct_locked = self.rl.record_failure_account(&self.subject);
            if acct_locked {
                tracing::warn!(
                    "SECURITY | event={}_brute_force_lockout | scope=account | subject={} | ip={}",
                    self.event_prefix,
                    sanitize_for_log(&self.subject),
                    self.ip
                );
            }
        }
    }
}

/// Sanitise a subject value before logging — strip CR/LF/NUL so the
/// caller-supplied AKID can't smuggle log-line injection.
fn sanitize_for_log(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Serialise every test that mutates the `DGP_TRUST_PROXY_HEADERS`
    /// env var. Cargo runs tests in parallel within a crate, and env
    /// vars are process-global, so without this lock two sibling
    /// tests can clobber each other's `set_var` / `remove_var` calls.
    /// (Race cause: the XFF-ignores-by-default test reads `false`
    /// expected, but sees `true` because the v4-mapped test hasn't
    /// `remove_var`'d yet.)
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        // `poisoned` intentionally swallowed — the test's env_var
        // mutation is idempotent on cleanup, so a poisoned lock just
        // means a previous test panicked. We still want to serialise.
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_extract_client_ip_ignores_xff_by_default() {
        let _g = env_lock();
        // Defensive: ensure the var is not set when we enter (a prior
        // test may have left it set if it panicked before cleanup).
        std::env::remove_var("DGP_TRUST_PROXY_HEADERS");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let ip = extract_client_ip(&headers);
        assert_eq!(
            ip, None,
            "XFF should be ignored by default (DGP_TRUST_PROXY_HEADERS=false)"
        );
    }

    #[test]
    fn test_extract_client_ip_without_headers() {
        let headers = axum::http::HeaderMap::new();
        let ip = extract_client_ip(&headers);
        assert_eq!(ip, None, "should return None when no proxy headers present");
    }

    #[test]
    fn test_extract_client_ip_uses_peer_when_proxy_headers_untrusted() {
        let _g = env_lock();
        std::env::remove_var("DGP_TRUST_PROXY_HEADERS");
        let headers = axum::http::HeaderMap::new();
        let peer = Some(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)));
        assert_eq!(extract_client_ip_with_peer(&headers, peer), peer);
    }

    #[test]
    fn test_allows_under_limit() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60), Duration::from_secs(120));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        assert!(!limiter.is_limited(&ip));
        assert!(!limiter.record_failure(&ip)); // 1st failure
        assert!(!limiter.record_failure(&ip)); // 2nd failure
        assert!(!limiter.is_limited(&ip));
    }

    #[test]
    fn test_blocks_at_limit() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60), Duration::from_secs(120));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        assert!(!limiter.record_failure(&ip)); // 1
        assert!(!limiter.record_failure(&ip)); // 2
        assert!(limiter.record_failure(&ip)); // 3 — now locked
        assert!(limiter.is_limited(&ip));
    }

    #[test]
    fn test_success_resets() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60), Duration::from_secs(120));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        limiter.record_failure(&ip);
        limiter.record_failure(&ip);
        limiter.record_success(&ip);
        assert!(!limiter.is_limited(&ip));
        assert!(!limiter.record_failure(&ip)); // Counter reset
    }

    #[test]
    fn test_different_ips_independent() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60), Duration::from_secs(120));
        let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

        assert!(!limiter.record_failure(&ip1)); // 1st for ip1
        assert!(limiter.record_failure(&ip1)); // 2nd for ip1 — locked
        assert!(!limiter.is_limited(&ip2)); // ip2 unaffected
        assert!(!limiter.record_failure(&ip2)); // 1st for ip2 — ok
    }

    #[test]
    fn test_cleanup_expired() {
        let limiter = RateLimiter::new(3, Duration::from_millis(10), Duration::from_millis(10));
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1));

        limiter.record_failure(&ip);
        assert_eq!(limiter.entries.len(), 1);

        // Wait for window to expire
        std::thread::sleep(Duration::from_millis(20));
        limiter.cleanup_expired();
        assert_eq!(limiter.entries.len(), 0);
    }

    #[test]
    fn test_normalize_ipv4_mapped_ipv6_collapses_to_v4() {
        // The two representations of the same host must hash into the same
        // rate-limit bucket; otherwise an attacker doubles their brute-force
        // budget by alternating forms in X-Forwarded-For.
        let v4 = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        let mapped: IpAddr = "::ffff:1.2.3.4".parse().unwrap();
        assert!(matches!(mapped, IpAddr::V6(_))); // precondition
        assert_eq!(normalize_ip(mapped), v4);
        assert_eq!(normalize_ip(v4), v4);
    }

    #[test]
    fn test_normalize_ip_passes_through_non_mapped_v6() {
        let pure_v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert_eq!(normalize_ip(pure_v6), pure_v6);
    }

    #[test]
    fn test_extract_client_ip_collapses_v4_mapped_from_xff() {
        let _g = env_lock();
        std::env::set_var("DGP_TRUST_PROXY_HEADERS", "true");
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "::ffff:1.2.3.4".parse().unwrap());
        let ip = extract_client_ip(&headers).unwrap();
        assert_eq!(ip, IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)));
        std::env::remove_var("DGP_TRUST_PROXY_HEADERS");
    }

    /// Per-account bucket: same shape as per-IP but keyed on subject.
    /// `record_failure_account` returns true when lockout triggers.
    #[test]
    fn account_bucket_independent_from_ip_bucket() {
        let limiter = RateLimiter::new(100, Duration::from_secs(60), Duration::from_secs(120))
            .with_account_policy(3, Duration::from_secs(60), Duration::from_secs(120));
        let ip = IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1));

        assert!(!limiter.is_limited(&ip));
        assert!(!limiter.is_limited_account("bootstrap"));

        // 3 failures against the same subject — even from different
        // IPs would trip the account bucket. We use the same IP here
        // just to prove the per-IP bucket is far from the per-IP cap
        // (100) yet the per-account bucket (3) hits lockout.
        for _ in 0..2 {
            assert!(!limiter.record_failure_account("bootstrap"));
        }
        assert!(limiter.record_failure_account("bootstrap"));
        assert!(limiter.is_limited_account("bootstrap"));
        // Per-IP bucket was NOT touched — separate dimension.
        assert!(!limiter.is_limited(&ip));
    }

    /// Empty subject — caller didn't supply one — must short-circuit
    /// to "not limited" so endpoints that don't have an account
    /// dimension don't false-trigger.
    #[test]
    fn account_bucket_empty_subject_is_no_op() {
        let limiter = RateLimiter::new(100, Duration::from_secs(60), Duration::from_secs(120))
            .with_account_policy(1, Duration::from_secs(60), Duration::from_secs(120));
        assert!(!limiter.is_limited_account(""));
        // Failure with empty subject must NOT enter the map.
        assert!(!limiter.record_failure_account(""));
        assert!(!limiter.is_limited_account(""));
    }

    /// Adversarial: distributed brute force against the SAME account
    /// from many IPs should be caught by the account bucket even
    /// when the per-IP bucket is wide open.
    #[test]
    fn account_bucket_catches_distributed_brute_force() {
        let limiter = RateLimiter::new(
            1000, // wide-open per-IP — irrelevant
            Duration::from_secs(60),
            Duration::from_secs(120),
        )
        .with_account_policy(5, Duration::from_secs(60), Duration::from_secs(120));

        // 5 different attacker IPs would each make 1 attempt against
        // the same subject. We don't need to materialise distinct
        // IpAddrs here: the per-IP bucket is wide open (1000) so
        // even hitting the SAME IP 5 times wouldn't trip it, and the
        // per-account bucket is keyed purely on the subject string.
        // The point of this test is that the account bucket trips
        // on the 5th attempt regardless of IP cardinality.
        for i in 1..=5u8 {
            let locked = limiter.record_failure_account("admin-akid");
            if i < 5 {
                assert!(!locked, "attempt {i} should not lock");
            } else {
                assert!(locked, "5th attempt must trip the account lockout");
            }
        }
        assert!(limiter.is_limited_account("admin-akid"));
        // A DIFFERENT subject is untouched — the lockout is account-scoped.
        assert!(!limiter.is_limited_account("other-akid"));
    }

    /// record_success on the guard clears BOTH the per-IP and the
    /// per-account counters so a legitimate login fully rotates the
    /// bucket state.
    #[test]
    fn account_bucket_success_clears_failures() {
        let limiter = RateLimiter::new(100, Duration::from_secs(60), Duration::from_secs(120))
            .with_account_policy(3, Duration::from_secs(60), Duration::from_secs(120));
        limiter.record_failure_account("alice");
        limiter.record_failure_account("alice");
        limiter.record_success_account("alice");
        // Counter reset; next failure starts from 1.
        assert!(!limiter.record_failure_account("alice"));
    }
}
