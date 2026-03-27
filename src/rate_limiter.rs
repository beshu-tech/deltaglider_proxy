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

    /// Create a rate limiter with default security settings:
    /// 5 attempts per 15-minute window, 30-minute lockout.
    pub fn default_auth() -> Self {
        Self::new(
            5,
            Duration::from_secs(15 * 60),
            Duration::from_secs(30 * 60),
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

/// Extract client IP from request headers/connection info.
/// Checks X-Forwarded-For first (for reverse proxies), then falls back to ConnectInfo.
pub fn extract_client_ip(headers: &axum::http::HeaderMap) -> Option<IpAddr> {
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

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
