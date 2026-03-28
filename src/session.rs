//! In-memory session store for admin GUI authentication.

use parking_lot::RwLock;
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Maximum number of concurrent sessions. Oldest sessions are evicted on overflow.
const MAX_SESSIONS: usize = 10;

/// Default session TTL: 4 hours.
/// Overridable at startup via `DGP_SESSION_TTL_HOURS` env var.
fn default_session_ttl() -> Duration {
    let hours: u64 = std::env::var("DGP_SESSION_TTL_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    Duration::from_secs(hours * 3600)
}

/// S3 credentials stored in a server-side session.
/// Held in memory only — never written to disk or localStorage.
#[derive(Clone, Serialize, Deserialize)]
pub struct S3SessionCredentials {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl Drop for S3SessionCredentials {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // Zero out the secret on drop to prevent lingering in memory.
        // SAFETY: we own the String and are about to drop it; overwriting
        // the UTF-8 bytes with zeros is safe (zeros are valid UTF-8).
        unsafe {
            self.secret_access_key
                .as_bytes_mut()
                .iter_mut()
                .for_each(|b| *b = 0);
        }
    }
}

struct SessionInfo {
    created_at: Instant,
    ip: Option<IpAddr>,
    s3_creds: Option<S3SessionCredentials>,
}

/// Thread-safe in-memory session store.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionInfo>>,
    ttl: Duration,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl: default_session_ttl(),
        }
    }

    /// The configured session TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Create a new session and return the token (64-char hex string).
    /// Stores the client IP for later validation.
    /// If the maximum number of concurrent sessions is reached, the oldest session is evicted.
    pub fn create_session(&self, ip: Option<IpAddr>) -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill(&mut bytes);
        let token = hex::encode(bytes);

        let mut sessions = self.sessions.write();

        // Evict oldest session if at capacity
        while sessions.len() >= MAX_SESSIONS {
            if let Some(oldest_token) = sessions
                .iter()
                .min_by_key(|(_, info)| info.created_at)
                .map(|(token, _)| token.clone())
            {
                tracing::debug!(
                    "Evicting oldest session to make room (max {})",
                    MAX_SESSIONS
                );
                sessions.remove(&oldest_token);
            } else {
                break;
            }
        }

        sessions.insert(
            token.clone(),
            SessionInfo {
                created_at: Instant::now(),
                ip,
                s3_creds: None,
            },
        );

        token
    }

    /// Check if a session token is valid (exists, not expired, and IP matches if stored).
    pub fn validate(&self, token: &str, ip: Option<IpAddr>) -> bool {
        let sessions = self.sessions.read();
        sessions
            .get(token)
            .map(|info| {
                if info.created_at.elapsed() >= self.ttl {
                    return false;
                }
                // If the session has a stored IP, the caller's IP must match
                if let (Some(stored_ip), Some(caller_ip)) = (info.ip, ip) {
                    if stored_ip != caller_ip {
                        tracing::warn!(
                            "Session IP mismatch: stored={}, caller={}",
                            stored_ip,
                            caller_ip
                        );
                        return false;
                    }
                }
                true
            })
            .unwrap_or(false)
    }

    /// Remove a session (logout).
    pub fn remove(&self, token: &str) {
        self.sessions.write().remove(token);
    }

    /// Store S3 credentials in an existing session.
    pub fn set_s3_creds(&self, token: &str, creds: S3SessionCredentials) {
        let mut sessions = self.sessions.write();
        if let Some(info) = sessions.get_mut(token) {
            info.s3_creds = Some(creds);
        }
    }

    /// Retrieve S3 credentials from a session (if present and session is valid).
    pub fn get_s3_creds(&self, token: &str) -> Option<S3SessionCredentials> {
        let sessions = self.sessions.read();
        sessions.get(token).and_then(|info| {
            if info.created_at.elapsed() >= self.ttl {
                return None;
            }
            info.s3_creds.clone()
        })
    }

    /// Clear S3 credentials from a session.
    pub fn clear_s3_creds(&self, token: &str) {
        let mut sessions = self.sessions.write();
        if let Some(info) = sessions.get_mut(token) {
            info.s3_creds = None;
        }
    }

    /// Remove all expired sessions.
    pub fn cleanup_expired(&self) {
        let ttl = self.ttl;
        self.sessions
            .write()
            .retain(|_, info| info.created_at.elapsed() < ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_validate() {
        let store = SessionStore::new();
        let token = store.create_session(None);
        assert_eq!(token.len(), 64);
        assert!(store.validate(&token, None));
    }

    #[test]
    fn test_invalid_token() {
        let store = SessionStore::new();
        assert!(!store.validate("nonexistent", None));
    }

    #[test]
    fn test_remove() {
        let store = SessionStore::new();
        let token = store.create_session(None);
        assert!(store.validate(&token, None));
        store.remove(&token);
        assert!(!store.validate(&token, None));
    }

    #[test]
    fn test_ip_binding() {
        let store = SessionStore::new();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();

        let token = store.create_session(Some(ip1));

        // Same IP works
        assert!(store.validate(&token, Some(ip1)));
        // Different IP rejected
        assert!(!store.validate(&token, Some(ip2)));
        // No caller IP provided — passes (graceful for proxies that strip IP)
        assert!(store.validate(&token, None));
    }

    #[test]
    fn test_max_sessions_eviction() {
        let store = SessionStore::new();
        let mut tokens = Vec::new();
        for _ in 0..MAX_SESSIONS {
            tokens.push(store.create_session(None));
        }

        // All sessions valid
        for t in &tokens {
            assert!(store.validate(t, None));
        }

        // Add one more — oldest should be evicted
        let new_token = store.create_session(None);
        assert!(store.validate(&new_token, None));
        assert!(!store.validate(&tokens[0], None)); // oldest evicted
        assert_eq!(store.sessions.read().len(), MAX_SESSIONS);
    }
}
