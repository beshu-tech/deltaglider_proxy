// SPDX-License-Identifier: GPL-3.0-only

//! In-memory session store for admin GUI authentication.

use parking_lot::RwLock;
use rand::rngs::OsRng;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use zeroize::Zeroize;

/// Maximum number of concurrent sessions. Oldest sessions are evicted on overflow.
const MAX_SESSIONS: usize = 10;

/// Default session TTL: 4 hours.
/// Overridable at startup via `DGP_SESSION_TTL_HOURS` env var.
fn default_session_ttl() -> Duration {
    let hours: u64 = crate::config::env_parse_with_default("DGP_SESSION_TTL_HOURS", 4);
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

impl S3SessionCredentials {
    /// Sentinel key pair used by open-mode (`authentication: none`) browser
    /// sessions, where the proxy has no real SigV4 keys. THE single home for
    /// the `"anonymous"` literal — call sites must not duplicate it.
    pub const ANONYMOUS_KEY: &'static str = "anonymous";

    /// Build open-mode anonymous S3 credentials. The access/secret pair is the
    /// [`ANONYMOUS_KEY`](Self::ANONYMOUS_KEY) sentinel; endpoint/region/bucket
    /// come from the caller.
    pub fn anonymous(endpoint: String, region: String, bucket: String) -> Self {
        S3SessionCredentials {
            endpoint,
            region,
            bucket,
            access_key_id: Self::ANONYMOUS_KEY.to_string(),
            secret_access_key: Self::ANONYMOUS_KEY.to_string(),
        }
    }
}

impl Drop for S3SessionCredentials {
    fn drop(&mut self) {
        // Zero out the secret on drop to prevent it from lingering in memory
        // after the session is invalidated. Uses the `zeroize` crate which is
        // designed for this purpose and avoids the need for unsafe code.
        self.secret_access_key.zeroize();
    }
}

/// How the admin session was created (for audit logging and UI display).
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// Bootstrap password login.
    Bootstrap,
    /// IAM user login via access key + secret.
    IamLoginAs { access_key_id: String },
    /// IAM user browser connect (non-admin): cookie + stored S3 creds only.
    IamBrowserLift { access_key_id: String },
    /// Open-auth mode: anonymous S3 browser session (no IAM identity).
    OpenLift,
    /// External provider login (OAuth/OIDC).
    External { provider_name: String, user_id: i64 },
}

/// Whether a session may call full admin GUI APIs (config, IAM, operator tools).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// Config, IAM, diagnostics, usage scanner, etc.
    AdminGui,
    /// S3 browser only: session check, logout, stored S3 credentials — no admin surface.
    S3BrowserLift,
}

struct SessionInfo {
    created_at: Instant,
    ip: Option<IpAddr>,
    s3_creds: Option<S3SessionCredentials>,
    auth_method: AuthMethod,
    kind: SessionKind,
}

/// Redacted view of a live session for the admin revocation UI. Never carries
/// the auth token or any S3 secret.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// Non-secret short id (token prefix) — used to target revocation.
    pub id: String,
    pub ip: Option<String>,
    pub age_secs: u64,
    pub admin_gui: bool,
    /// Auth kind: bootstrap / iam / iam_browser / open / external.
    pub auth: String,
    /// access_key_id (IAM) or provider name (external); None for bootstrap/open.
    pub identity: Option<String>,
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

    fn entry_valid(&self, info: &SessionInfo, ip: Option<IpAddr>) -> bool {
        if info.created_at.elapsed() >= self.ttl {
            return false;
        }
        if let Some(stored_ip) = info.ip {
            match ip {
                Some(caller_ip) if caller_ip == stored_ip => {}
                Some(caller_ip) => {
                    tracing::warn!(
                        "Session IP mismatch: stored={}, caller={}",
                        stored_ip,
                        caller_ip
                    );
                    return false;
                }
                None => {
                    tracing::warn!(
                        "Session has IP binding ({}) but caller provided no IP",
                        stored_ip
                    );
                    return false;
                }
            }
        }
        true
    }

    /// Create a new session and return the token (64-char hex string).
    /// Stores the client IP for later validation.
    /// If the maximum number of concurrent sessions is reached, the oldest session is evicted.
    pub fn create_session(
        &self,
        ip: Option<IpAddr>,
        auth_method: AuthMethod,
        kind: SessionKind,
    ) -> String {
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
                tracing::warn!(
                    "Evicting oldest admin session to make room (max {})",
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
                auth_method,
                kind,
            },
        );

        token
    }

    /// Check if a session token is valid (exists, not expired, and IP matches if stored).
    pub fn validate(&self, token: &str, ip: Option<IpAddr>) -> bool {
        let sessions = self.sessions.read();
        sessions
            .get(token)
            .map(|info| self.entry_valid(info, ip))
            .unwrap_or(false)
    }

    /// Full admin GUI (config, IAM, operator APIs). `S3BrowserLift` sessions return false.
    pub fn allows_admin_gui(&self, token: &str, ip: Option<IpAddr>) -> bool {
        let sessions = self.sessions.read();
        let Some(info) = sessions.get(token) else {
            return false;
        };
        if !self.entry_valid(info, ip) {
            return false;
        }
        info.kind == SessionKind::AdminGui
    }

    /// Remove a session (logout).
    pub fn remove(&self, token: &str) {
        self.sessions.write().remove(token);
    }

    /// A non-secret session id derived from the token — the first 12 hex chars.
    /// Enough to identify a session in the admin list and target it for
    /// revocation without exposing the full 64-char auth token.
    fn session_id(token: &str) -> String {
        token.chars().take(12).collect()
    }

    /// List live (non-expired) sessions for the admin revocation UI. Redacted:
    /// never returns the token or S3 secret — only a short id, IP, age, kind,
    /// and the auth identity (access key / provider).
    pub fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.read();
        sessions
            .iter()
            .filter(|(_, info)| info.created_at.elapsed() < self.ttl)
            .map(|(token, info)| {
                let (auth, identity) = match &info.auth_method {
                    AuthMethod::Bootstrap => ("bootstrap", None),
                    AuthMethod::IamLoginAs { access_key_id } => {
                        ("iam", Some(access_key_id.clone()))
                    }
                    AuthMethod::IamBrowserLift { access_key_id } => {
                        ("iam_browser", Some(access_key_id.clone()))
                    }
                    AuthMethod::OpenLift => ("open", None),
                    AuthMethod::External { provider_name, .. } => {
                        ("external", Some(provider_name.clone()))
                    }
                };
                SessionSummary {
                    id: Self::session_id(token),
                    ip: info.ip.map(|i| i.to_string()),
                    age_secs: info.created_at.elapsed().as_secs(),
                    admin_gui: info.kind == SessionKind::AdminGui,
                    auth: auth.to_string(),
                    identity,
                }
            })
            .collect()
    }

    /// True if `token` (a full session token) has the given short `id`. Used to
    /// stop an admin revoking their own session via the revoke-by-id route.
    pub fn session_id_matches(&self, token: &str, id: &str) -> bool {
        Self::session_id(token) == id
    }

    /// Revoke a session by its non-secret id (force-logout). Returns true if a
    /// session matched. The id is a token prefix; matching by prefix is safe
    /// because a 12-hex-char (48-bit) collision among ≤10 live sessions is
    /// negligible, and we only ever revoke server-held tokens.
    pub fn revoke_by_id(&self, id: &str) -> bool {
        let mut sessions = self.sessions.write();
        let targets: Vec<String> = sessions
            .keys()
            .filter(|t| Self::session_id(t) == id)
            .cloned()
            .collect();
        for t in &targets {
            sessions.remove(t);
        }
        !targets.is_empty()
    }

    /// Force-logout EVERY session of an IAM user (by access_key_id). Used when a
    /// key is rotated/compromised — rotating the key alone does NOT invalidate
    /// already-minted session cookies. Returns the count revoked.
    pub fn revoke_by_access_key(&self, access_key_id: &str) -> usize {
        let mut sessions = self.sessions.write();
        let targets: Vec<String> = sessions
            .iter()
            .filter(|(_, info)| match &info.auth_method {
                AuthMethod::IamLoginAs { access_key_id: k }
                | AuthMethod::IamBrowserLift { access_key_id: k } => k == access_key_id,
                _ => false,
            })
            .map(|(t, _)| t.clone())
            .collect();
        for t in &targets {
            sessions.remove(t);
        }
        targets.len()
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

    /// Get the auth method for a valid session token.
    pub fn auth_method(&self, token: &str, ip: Option<IpAddr>) -> Option<AuthMethod> {
        let sessions = self.sessions.read();
        sessions.get(token).and_then(|info| {
            if self.entry_valid(info, ip) {
                Some(info.auth_method.clone())
            } else {
                None
            }
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
        let token = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
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
        let token = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        assert!(store.validate(&token, None));
        store.remove(&token);
        assert!(!store.validate(&token, None));
    }

    #[test]
    fn test_ip_binding() {
        let store = SessionStore::new();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();

        let token = store.create_session(Some(ip1), AuthMethod::Bootstrap, SessionKind::AdminGui);

        // Same IP works
        assert!(store.validate(&token, Some(ip1)));
        // Different IP rejected
        assert!(!store.validate(&token, Some(ip2)));
        // No caller IP provided — rejected (session has IP binding)
        assert!(!store.validate(&token, None));
    }

    #[test]
    fn test_max_sessions_eviction() {
        let store = SessionStore::new();
        let mut tokens = Vec::new();
        for _ in 0..MAX_SESSIONS {
            tokens.push(store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui));
        }

        // All sessions valid
        for t in &tokens {
            assert!(store.validate(t, None));
        }

        // Add one more — oldest should be evicted
        let new_token = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        assert!(store.validate(&new_token, None));
        assert!(!store.validate(&tokens[0], None)); // oldest evicted
        assert_eq!(store.sessions.read().len(), MAX_SESSIONS);
    }

    // ── AuthMethod tests ──

    #[test]
    fn test_auth_method_bootstrap() {
        let store = SessionStore::new();
        let token = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        let method = store.auth_method(&token, None);
        assert!(matches!(method, Some(AuthMethod::Bootstrap)));
    }

    #[test]
    fn test_auth_method_iam_login_as() {
        let store = SessionStore::new();
        let token = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKTEST01".into(),
            },
            SessionKind::AdminGui,
        );
        let method = store.auth_method(&token, None).unwrap();
        match method {
            AuthMethod::IamLoginAs { access_key_id } => {
                assert_eq!(access_key_id, "AKTEST01");
            }
            _ => panic!("Expected IamLoginAs"),
        }
    }

    #[test]
    fn test_auth_method_external() {
        let store = SessionStore::new();
        let token = store.create_session(
            None,
            AuthMethod::External {
                provider_name: "google".into(),
                user_id: 42,
            },
            SessionKind::AdminGui,
        );
        let method = store.auth_method(&token, None).unwrap();
        match method {
            AuthMethod::External {
                provider_name,
                user_id,
            } => {
                assert_eq!(provider_name, "google");
                assert_eq!(user_id, 42);
            }
            _ => panic!("Expected External"),
        }
    }

    #[test]
    fn test_auth_method_none_for_invalid_token() {
        let store = SessionStore::new();
        assert!(store.auth_method("nonexistent", None).is_none());
    }

    #[test]
    fn test_auth_method_respects_ip_binding() {
        let store = SessionStore::new();
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let token = store.create_session(Some(ip1), AuthMethod::Bootstrap, SessionKind::AdminGui);
        assert!(matches!(
            store.auth_method(&token, Some(ip1)),
            Some(AuthMethod::Bootstrap)
        ));
        assert!(store.auth_method(&token, Some(ip2)).is_none());
    }

    #[test]
    fn test_allows_admin_gui_rejects_browser_lift() {
        let store = SessionStore::new();
        let admin_t = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        let lift_t = store.create_session(
            None,
            AuthMethod::IamBrowserLift {
                access_key_id: "AKX".into(),
            },
            SessionKind::S3BrowserLift,
        );
        assert!(store.allows_admin_gui(&admin_t, None));
        assert!(!store.allows_admin_gui(&lift_t, None));
    }

    #[test]
    fn test_list_and_revoke_sessions() {
        let store = SessionStore::new();
        let admin_t = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        let user_t = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKIA1".into(),
            },
            SessionKind::AdminGui,
        );

        // list() is redacted: short id, no token.
        let list = store.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|s| s.id.len() == 12));
        assert!(list.iter().any(|s| s.identity.as_deref() == Some("AKIA1")));

        // revoke_by_id force-logs-out the targeted session, leaves the other.
        let admin_id = SessionStore::session_id(&admin_t);
        assert!(store.revoke_by_id(&admin_id));
        assert!(!store.validate(&admin_t, None));
        assert!(store.validate(&user_t, None));
        assert!(!store.revoke_by_id("deadbeefdead"), "unknown id → false");

        // revoke_by_access_key kills every session of that IAM user.
        let n = store.revoke_by_access_key("AKIA1");
        assert_eq!(n, 1);
        assert!(!store.validate(&user_t, None));
    }
}
