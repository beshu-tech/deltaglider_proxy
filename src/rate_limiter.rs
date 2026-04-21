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
#[derive(Clone)]
pub struct RateLimiter {
    /// Map from IP to (failure_count, first_failure_time, lockout_start).
    entries: Arc<DashMap<IpAddr, RateLimitEntry>>,
    /// Maximum failed attempts before lockout.
    max_attempts: u32,
    /// Rolling window for counting attempts.
    window: Duration,
    /// Lockout duration after max_attempts exceeded.
    lockout: Duration,
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
    /// - `max_attempts`: max failures before lockout
    /// - `window`: time window for counting failures
    /// - `lockout`: lockout duration after exceeding max_attempts
    ///
    /// See `default_auth()` for production defaults (100 attempts / 5min / 10min).
    pub fn new(max_attempts: u32, window: Duration, lockout: Duration) -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            max_attempts,
            window,
            lockout,
        }
    }

    /// Create a rate limiter from environment variables with defaults:
    /// - `DGP_RATE_LIMIT_MAX_ATTEMPTS`: max failures before lockout (default: 100)
    /// - `DGP_RATE_LIMIT_WINDOW_SECS`: rolling window in seconds (default: 300 = 5 min)
    /// - `DGP_RATE_LIMIT_LOCKOUT_SECS`: lockout duration in seconds (default: 600 = 10 min)
    pub fn default_auth() -> Self {
        use crate::config::env_parse_with_default;
        let max_attempts: u32 = env_parse_with_default("DGP_RATE_LIMIT_MAX_ATTEMPTS", 100);
        let window_secs: u64 = env_parse_with_default("DGP_RATE_LIMIT_WINDOW_SECS", 300); // 5 minutes
        let lockout_secs: u64 = env_parse_with_default("DGP_RATE_LIMIT_LOCKOUT_SECS", 600); // 10 minutes
        tracing::info!(
            "Rate limiter: {} attempts per {}s window, {}s lockout",
            max_attempts,
            window_secs,
            lockout_secs
        );
        Self::new(
            max_attempts,
            Duration::from_secs(window_secs),
            Duration::from_secs(lockout_secs),
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
    /// After that, doubles each time: 100ms, 200ms, 400ms, 800ms, 1.6s, 3.2s, 5s.
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

    /// Remove expired entries to prevent unbounded memory growth.
    /// Call this periodically (e.g., every 5 minutes).
    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        let window = self.window;
        let lockout = self.lockout;

        self.entries.retain(|_ip, entry| {
            // Keep entries that are currently locked out and lockout hasn't expired
            if let Some(lockout_start) = entry.lockout_start {
                if now.duration_since(lockout_start) < lockout {
                    return true; // Keep: still locked out
                }
            }
            // Keep entries within the active window
            now.duration_since(entry.window_start) < window
        });
    }
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
/// `aws:SourceIp` IAM conditions.
///
/// TODO: add axum `ConnectInfo<SocketAddr>` support so the real peer IP is
/// always available and proxy-header trust is unnecessary for rate limiting.
fn trust_proxy_headers() -> bool {
    crate::config::env_bool("DGP_TRUST_PROXY_HEADERS", false)
}

/// Extract client IP from request headers/connection info.
///
/// When `DGP_TRUST_PROXY_HEADERS=true`, checks X-Forwarded-For and X-Real-IP
/// (for deployments behind a trusted reverse proxy). Otherwise ignores these
/// headers to prevent IP spoofing.
///
/// Returns `None` if no IP can be determined. In this case, rate limiting is
/// skipped for this request (the SigV4 signature check still applies).
/// To enable per-IP rate limiting without a reverse proxy, set
/// `DGP_TRUST_PROXY_HEADERS=true` and have your proxy set these headers,
/// or consider adding axum `ConnectInfo` support in the future.
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> Option<IpAddr> {
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

    None
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
#[derive(Debug, Clone, Copy)]
pub struct Blocked {
    pub ip: IpAddr,
    pub failure_count: u32,
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
    event_prefix: &'static str,
}

impl<'a> RateLimitGuard<'a> {
    /// Begin rate-limit-protected execution.
    ///
    /// 1. Extract client IP from headers (with UNSPECIFIED fallback so the
    ///    limiter still counts failures when proxy-header extraction is
    ///    disabled — shared-UNSPECIFIED-bucket behavior matches what each
    ///    hand-rolled site did before this abstraction).
    /// 2. Check lockout. On lockout: log a SECURITY event and return
    ///    `Err(Blocked { ip, failure_count })` — the caller must return
    ///    their 429 response without proceeding.
    /// 3. Apply the limiter's progressive delay (tokio sleep). This is
    ///    awaited inside `enter` so call sites stay simple.
    ///
    /// `event_prefix` names the origin surface (e.g. `"admin"`,
    /// `"login_as"`, `"s3_sigv4"`) and flows into security log events as
    /// `"{prefix}_brute_force_blocked"` / `"{prefix}_brute_force_lockout"`.
    pub async fn enter(
        rl: &'a RateLimiter,
        headers: &axum::http::HeaderMap,
        event_prefix: &'static str,
    ) -> Result<Self, Blocked> {
        let ip = extract_client_ip(headers).unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        if rl.is_limited(&ip) {
            let failure_count = rl.failure_count(&ip);
            tracing::warn!(
                "SECURITY | event={}_brute_force_blocked | ip={} | attempts={}",
                event_prefix,
                ip,
                failure_count
            );
            return Err(Blocked { ip, failure_count });
        }
        let delay = rl.progressive_delay(&ip);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        Ok(Self {
            rl,
            ip,
            event_prefix,
        })
    }

    /// The IP the guard is scoped to. Exposed for callers that need to
    /// include the IP in their response body or own audit logs.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// Record a successful operation. Resets the failure counter for this
    /// IP so future attempts start from zero.
    pub fn record_success(&self) {
        self.rl.record_success(&self.ip);
    }

    /// Record a failed operation. Increments the failure counter and, if
    /// this failure triggers lockout, emits a SECURITY log event tagged
    /// with `event_prefix` so operators can trace which endpoint a brute-
    /// force burst originated from.
    pub fn record_failure(&self) {
        let locked = self.rl.record_failure(&self.ip);
        if locked {
            let count = self.rl.failure_count(&self.ip);
            tracing::warn!(
                "SECURITY | event={}_brute_force_lockout | ip={} | attempts={}",
                self.event_prefix,
                self.ip,
                count
            );
        }
    }
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
}
