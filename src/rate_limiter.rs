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
    /// - `max_attempts`: max failures before lockout (default: 5)
    /// - `window`: time window for counting failures (default: 15 minutes)
    /// - `lockout`: lockout duration after exceeding max_attempts (default: 30 minutes)
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
        let max_attempts = std::env::var("DGP_RATE_LIMIT_MAX_ATTEMPTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100u32);
        let window_secs = std::env::var("DGP_RATE_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300u64); // 5 minutes
        let lockout_secs = std::env::var("DGP_RATE_LIMIT_LOCKOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600u64); // 10 minutes
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
/// Controlled by `DGP_TRUST_PROXY_HEADERS`. **Defaults to `true`** to preserve
/// behaviour from versions prior to v0.5.2, where proxy headers were always trusted.
///
/// **Security note**: the default of `true` is only safe when the proxy sits behind
/// a trusted reverse proxy (nginx, Caddy, ALB) that sets/overwrites these headers.
/// Direct-to-internet deployments should set `DGP_TRUST_PROXY_HEADERS=false`,
/// otherwise any client can spoof their IP to bypass rate limiting and poison
/// `aws:SourceIp` IAM conditions.
///
/// TODO: add axum `ConnectInfo<SocketAddr>` support so the real peer IP is
/// always available and proxy-header trust is unnecessary for rate limiting.
fn trust_proxy_headers() -> bool {
    std::env::var("DGP_TRUST_PROXY_HEADERS")
        .map(|v| v == "true" || v == "1")
        // Defaults to true for backwards compatibility with pre-v0.5.2 deployments.
        // See doc comment above for the security implications.
        .unwrap_or(true)
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
                        return Some(ip);
                    }
                }
            }
        }

        // Check X-Real-IP header
        if let Some(real_ip) = headers.get("x-real-ip") {
            if let Ok(ip_str) = real_ip.to_str() {
                if let Ok(ip) = ip_str.trim().parse::<IpAddr>() {
                    return Some(ip);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn test_extract_client_ip_reads_xff_by_default() {
        // DGP_TRUST_PROXY_HEADERS defaults to true (preserves pre-v0.5.2 behaviour)
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        let ip = extract_client_ip(&headers);
        assert_eq!(
            ip,
            Some(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))),
            "XFF should be trusted by default"
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
}
