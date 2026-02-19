//! In-memory session store for admin GUI authentication.

use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Session TTL: 24 hours.
const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

struct SessionInfo {
    created_at: Instant,
}

/// Thread-safe in-memory session store.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionInfo>>,
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
        }
    }

    /// Create a new session and return the token (64-char hex string).
    pub fn create_session(&self) -> String {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill(&mut bytes);
        let token = hex::encode(bytes);

        self.sessions.write().insert(
            token.clone(),
            SessionInfo {
                created_at: Instant::now(),
            },
        );

        token
    }

    /// Check if a session token is valid (exists and not expired).
    pub fn validate(&self, token: &str) -> bool {
        let sessions = self.sessions.read();
        sessions
            .get(token)
            .map(|info| info.created_at.elapsed() < SESSION_TTL)
            .unwrap_or(false)
    }

    /// Remove a session (logout).
    pub fn remove(&self, token: &str) {
        self.sessions.write().remove(token);
    }

    /// Remove all expired sessions.
    pub fn cleanup_expired(&self) {
        self.sessions
            .write()
            .retain(|_, info| info.created_at.elapsed() < SESSION_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_validate() {
        let store = SessionStore::new();
        let token = store.create_session();
        assert_eq!(token.len(), 64);
        assert!(store.validate(&token));
    }

    #[test]
    fn test_invalid_token() {
        let store = SessionStore::new();
        assert!(!store.validate("nonexistent"));
    }

    #[test]
    fn test_remove() {
        let store = SessionStore::new();
        let token = store.create_session();
        assert!(store.validate(&token));
        store.remove(&token);
        assert!(!store.validate(&token));
    }
}
