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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
    /// Wall-clock creation time (unix seconds) — compared against the synced
    /// revocation epoch (`created_unix <= revoked_since` ⇒ invalid). `created_at`
    /// is monotonic and can't be compared to a wall-clock timestamp.
    created_unix: i64,
    ip: Option<IpAddr>,
    s3_creds: Option<S3SessionCredentials>,
    auth_method: AuthMethod,
    kind: SessionKind,
}

impl AuthMethod {
    /// The identity a cross-instance revocation targets: the IAM access_key_id,
    /// or `provider:user_id` for external logins. Bootstrap/open have no
    /// revocable identity (None) — those are cleared by restart / password reset.
    fn revocation_identity(&self) -> Option<String> {
        match self {
            AuthMethod::IamLoginAs { access_key_id }
            | AuthMethod::IamBrowserLift { access_key_id } => Some(access_key_id.clone()),
            AuthMethod::External {
                provider_name,
                user_id,
            } => Some(format!("{provider_name}:{user_id}")),
            AuthMethod::Bootstrap | AuthMethod::OpenLift => None,
        }
    }
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
    /// Revocation identity: access_key_id (IAM) or `provider:user_id`
    /// (external); None for bootstrap/open.
    pub identity: Option<String>,
}

/// Thread-safe in-memory session store.
pub struct SessionStore {
    sessions: RwLock<HashMap<String, SessionInfo>>,
    ttl: Duration,
    /// identity → revoke epoch (unix secs). A session with
    /// `created_unix <= revoked_since` is invalid on EVERY instance. Snapshotted
    /// from the synced `session_revocations` table (refreshed on revoke + after
    /// config sync) so `entry_valid` stays a pure in-memory check.
    revocations: RwLock<HashMap<String, i64>>,
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
            revocations: RwLock::new(HashMap::new()),
        }
    }

    /// MERGE a revocation snapshot (from the synced `session_revocations`
    /// table) monotonically. Called on startup, after each revoke, and after a
    /// config sync. A wholesale replace would un-revoke a compromised session:
    /// a revocation recorded locally via `note_revocation` but not yet in the
    /// incoming snapshot would be wiped, resurrecting the session (X-ray H20).
    /// So we keep the MAX epoch per identity and never DROP an identity the
    /// snapshot omits — revocations only ever advance, never regress.
    pub fn set_revocations(&self, rows: Vec<(String, i64)>) {
        let mut map = self.revocations.write();
        for (identity, revoked_since) in rows {
            let e = map.entry(identity).or_insert(revoked_since);
            *e = (*e).max(revoked_since);
        }
    }

    /// Record a local revocation immediately (so the current instance rejects the
    /// identity without waiting for a DB round-trip / sync). MAX so it's monotonic.
    pub fn note_revocation(&self, identity: &str, revoked_since: i64) {
        let mut r = self.revocations.write();
        let e = r.entry(identity.to_string()).or_insert(revoked_since);
        *e = (*e).max(revoked_since);
    }

    /// The configured session TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// TTL + cross-instance revocation-epoch check (no IP binding) — the
    /// "does this session still exist anywhere" half of `entry_valid`.
    fn entry_live(&self, info: &SessionInfo) -> bool {
        if info.created_at.elapsed() >= self.ttl {
            return false;
        }
        // Cross-instance revocation: if this session's identity was revoked at or
        // after the session was created, it's invalid — even if it was minted on
        // another node (the stolen-cookie / compromised-key escape hatch).
        if let Some(identity) = info.auth_method.revocation_identity() {
            if let Some(&revoked_since) = self.revocations.read().get(&identity) {
                if info.created_unix <= revoked_since {
                    return false;
                }
            }
        }
        true
    }

    fn entry_valid(&self, info: &SessionInfo, ip: Option<IpAddr>) -> bool {
        if !self.entry_live(info) {
            return false;
        }
        if !ip_ok(info.ip, ip) {
            match (info.ip, ip) {
                (Some(stored_ip), Some(caller_ip)) => tracing::warn!(
                    "Session IP mismatch: stored={}, caller={}",
                    stored_ip,
                    caller_ip
                ),
                (Some(stored_ip), None) => tracing::warn!(
                    "Session has IP binding ({}) but caller provided no IP",
                    stored_ip
                ),
                _ => {}
            }
            return false;
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
                created_unix: now_unix(),
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

    /// List live (non-expired, non-revoked) sessions for the admin revocation
    /// UI. Redacted: never returns the token or S3 secret — only a short id,
    /// IP, age, kind, and the revocation identity (access key / provider:user_id).
    pub fn list(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.read();
        sessions
            .iter()
            .filter(|(_, info)| self.entry_live(info))
            .map(|(token, info)| {
                let auth = match &info.auth_method {
                    AuthMethod::Bootstrap => "bootstrap",
                    AuthMethod::IamLoginAs { .. } => "iam",
                    AuthMethod::IamBrowserLift { .. } => "iam_browser",
                    AuthMethod::OpenLift => "open",
                    AuthMethod::External { .. } => "external",
                };
                SessionSummary {
                    id: Self::session_id(token),
                    ip: info.ip.map(|i| i.to_string()),
                    age_secs: info.created_at.elapsed().as_secs(),
                    admin_gui: info.kind == SessionKind::AdminGui,
                    auth: auth.to_string(),
                    // The same string revoke-by-identity matches on, so what the
                    // admin sees in the table is exactly what they can revoke.
                    identity: info.auth_method.revocation_identity(),
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

    /// Force-logout EVERY session matching a revocation identity (IAM
    /// access_key_id or `provider:user_id` for external logins). Used when a
    /// key is compromised — rotating the key alone does NOT invalidate
    /// already-minted session cookies. Returns the count revoked.
    pub fn revoke_by_identity(&self, identity: &str) -> usize {
        let mut sessions = self.sessions.write();
        let targets: Vec<String> = sessions
            .iter()
            .filter(|(_, info)| info.auth_method.revocation_identity().as_deref() == Some(identity))
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

    /// Retrieve S3 credentials from a session — full validity gate (TTL +
    /// revocation epoch + IP binding), same as `validate`.
    pub fn get_s3_creds(&self, token: &str, ip: Option<IpAddr>) -> Option<S3SessionCredentials> {
        let sessions = self.sessions.read();
        sessions.get(token).and_then(|info| {
            if !self.entry_valid(info, ip) {
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
        // Drop both TTL-expired AND revoked sessions. entry_live checks both, so
        // a revoked-but-not-yet-TTL-expired session is evicted here rather than
        // lingering in memory until its TTL — defense in depth so no future
        // epoch-handling bug can resurrect a session revoked long ago.
        self.sessions
            .write()
            .retain(|_, info| self.entry_live(info));
    }
}

/// Pure IP-binding check: a session bound to an IP is only valid for a caller
/// presenting that same IP; unbound sessions accept any caller.
fn ip_ok(stored: Option<IpAddr>, caller: Option<IpAddr>) -> bool {
    match stored {
        None => true,
        Some(stored) => caller == Some(stored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl SessionStore {
        /// Test accessor: the revocation epoch for an identity, if any.
        fn revoked_epoch(&self, identity: &str) -> Option<i64> {
            self.revocations.read().get(identity).copied()
        }
    }

    /// X-ray H20: a config-sync snapshot must MERGE monotonically, never
    /// replace. A locally-recorded revocation that the incoming snapshot omits
    /// must survive — else a compromised session is silently un-revoked.
    #[test]
    fn set_revocations_merges_monotonically_and_never_unrevokes() {
        let store = SessionStore::new();
        // Local revocation (e.g. operator revoked a compromised key here).
        store.note_revocation("iam:alice", 1000);
        // A sync snapshot arrives that does NOT contain alice (stale peer / not
        // yet propagated) but adds bob.
        store.set_revocations(vec![("iam:bob".into(), 500)]);
        assert_eq!(
            store.revoked_epoch("iam:alice"),
            Some(1000),
            "local revocation must NOT be wiped by a snapshot that omits it"
        );
        assert_eq!(store.revoked_epoch("iam:bob"), Some(500));
        // A later snapshot with a HIGHER epoch for alice advances it; a lower
        // one never regresses it.
        store.set_revocations(vec![("iam:alice".into(), 2000)]);
        assert_eq!(store.revoked_epoch("iam:alice"), Some(2000));
        store.set_revocations(vec![("iam:alice".into(), 100)]);
        assert_eq!(
            store.revoked_epoch("iam:alice"),
            Some(2000),
            "a lower incoming epoch must not regress a revocation"
        );
    }

    /// Session-cleanup critic-gap: cleanup_expired must EVICT a revoked session,
    /// not just leave entry_live to reject it — so no future epoch-handling bug
    /// can resurrect a long-revoked session lingering in the map.
    #[test]
    fn cleanup_expired_evicts_revoked_sessions() {
        let store = SessionStore::new();
        let token = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKIACOMPROMISED".into(),
            },
            SessionKind::AdminGui,
        );
        assert!(store.validate(&token, None), "fresh session is valid");
        assert_eq!(store.sessions.read().len(), 1);

        // Revoke the identity at a high epoch (>= the session's created_unix).
        store.note_revocation("AKIACOMPROMISED", i64::MAX);
        assert!(!store.validate(&token, None), "revoked session is invalid");

        // cleanup_expired must physically remove it, not just keep it invalid.
        store.cleanup_expired();
        assert_eq!(
            store.sessions.read().len(),
            0,
            "a revoked session must be evicted by cleanup, not linger until TTL"
        );
    }

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

        // revoke_by_identity kills every session of that IAM user.
        let n = store.revoke_by_identity("AKIA1");
        assert_eq!(n, 1);
        assert!(!store.validate(&user_t, None));
    }

    #[test]
    fn ip_ok_truth_table() {
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        // Unbound session: any caller (including none) is fine.
        assert!(ip_ok(None, None));
        assert!(ip_ok(None, Some(a)));
        // Bound session: only the exact same IP passes.
        assert!(ip_ok(Some(a), Some(a)));
        assert!(!ip_ok(Some(a), Some(b)));
        assert!(!ip_ok(Some(a), None));
    }

    #[test]
    fn revoke_by_identity_matches_iam_and_external_but_not_bootstrap() {
        let store = SessionStore::new();
        let login_t = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKZZ".into(),
            },
            SessionKind::AdminGui,
        );
        let lift_t = store.create_session(
            None,
            AuthMethod::IamBrowserLift {
                access_key_id: "AKZZ".into(),
            },
            SessionKind::S3BrowserLift,
        );
        let boot_t = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        let ext_t = store.create_session(
            None,
            AuthMethod::External {
                provider_name: "okta".into(),
                user_id: 7,
            },
            SessionKind::AdminGui,
        );

        // Both IAM variants match the access key; bootstrap/external untouched.
        assert_eq!(store.revoke_by_identity("AKZZ"), 2);
        assert!(!store.validate(&login_t, None));
        assert!(!store.validate(&lift_t, None));
        assert!(store.validate(&boot_t, None));
        assert!(store.validate(&ext_t, None));

        // External sessions are revocable via provider:user_id.
        assert_eq!(store.revoke_by_identity("okta:7"), 1);
        assert!(!store.validate(&ext_t, None));

        // Bootstrap has no revocation identity — nothing to match.
        assert_eq!(store.revoke_by_identity("bootstrap"), 0);
        assert!(store.validate(&boot_t, None));
    }

    #[test]
    fn list_omits_revoked_and_shows_external_identity() {
        let store = SessionStore::new();
        store.create_session(
            None,
            AuthMethod::External {
                provider_name: "okta".into(),
                user_id: 7,
            },
            SessionKind::AdminGui,
        );
        store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKL1".into(),
            },
            SessionKind::AdminGui,
        );

        // External rows expose the FULL provider:user_id revocation identity.
        let list = store.list();
        assert_eq!(list.len(), 2);
        assert!(list.iter().any(|s| s.identity.as_deref() == Some("okta:7")));

        // A revocation epoch at/after creation hides the session from list().
        store.note_revocation("AKL1", now_unix() + 1);
        let list = store.list();
        assert_eq!(list.len(), 1);
        assert!(list.iter().all(|s| s.identity.as_deref() != Some("AKL1")));
    }

    #[test]
    fn get_s3_creds_gated_on_revocation_and_ip() {
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let store = SessionStore::new();
        let token = store.create_session(
            Some(ip1),
            AuthMethod::IamBrowserLift {
                access_key_id: "AKS3".into(),
            },
            SessionKind::S3BrowserLift,
        );
        store.set_s3_creds(
            &token,
            S3SessionCredentials {
                endpoint: "http://localhost:9000".into(),
                region: "us-east-1".into(),
                bucket: "b".into(),
                access_key_id: "AKS3".into(),
                secret_access_key: "sekrit".into(),
            },
        );

        assert!(store.get_s3_creds(&token, Some(ip1)).is_some());
        // Wrong / missing caller IP: the stored secret must not come back.
        assert!(store.get_s3_creds(&token, Some(ip2)).is_none());
        assert!(store.get_s3_creds(&token, None).is_none());

        // Revoked identity: creds gone even from the right IP.
        store.note_revocation("AKS3", now_unix() + 1);
        assert!(store.get_s3_creds(&token, Some(ip1)).is_none());
    }

    #[test]
    fn cross_instance_revocation_snapshot_invalidates_by_identity() {
        let store = SessionStore::new();
        // A session minted "on another node" — model it via a normal create.
        let t = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKIA9".into(),
            },
            SessionKind::AdminGui,
        );
        assert!(store.validate(&t, None), "valid before any revocation");

        // A revocation with epoch in the FUTURE (>= created_unix) invalidates it,
        // even though the session lives only in this store — this is what a synced
        // revocation from another instance looks like after set_revocations().
        let future = now_unix() + 3600;
        store.set_revocations(vec![("AKIA9".to_string(), future)]);
        assert!(!store.validate(&t, None), "revoked identity is rejected");

        // A different identity's session is untouched.
        let other = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKIB0".into(),
            },
            SessionKind::AdminGui,
        );
        assert!(store.validate(&other, None));

        // A revocation epoch BEFORE the session was created does NOT invalidate a
        // session created afterwards (revocation only kills pre-existing sessions).
        let store2 = SessionStore::new();
        store2.set_revocations(vec![("AKIC1".to_string(), now_unix() - 3600)]);
        let fresh = store2.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: "AKIC1".into(),
            },
            SessionKind::AdminGui,
        );
        assert!(
            store2.validate(&fresh, None),
            "a session created AFTER the revoke epoch stays valid"
        );
    }
}
