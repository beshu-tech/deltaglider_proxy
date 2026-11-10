// SPDX-License-Identifier: BUSL-1.1

//! External authentication provider system (OAuth/OIDC, extensible to SAML/LDAP).
//!
//! This module provides a provider-agnostic external auth layer for the admin GUI.
//! External users authenticate via browser redirect (OAuth flow), and are auto-provisioned
//! as local IAM users with generated S3 credentials. The S3 SigV4 wire protocol is unchanged.

pub mod mapping;
pub mod oidc;
pub mod types;

use parking_lot::RwLock;
use rand::Rng;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config_db::auth_providers::AuthProviderConfig;

use self::oidc::OidcProvider;
use self::types::{AuthorizationRequest, ExternalAuthError, PendingAuth};

/// The one provider type the provider map loads.
pub const SUPPORTED_PROVIDER_TYPE: &str = "oidc";

/// Save-time check of a provider, for every entry point (admin API create
/// and update, declarative apply): the type must be `oidc` (another type
/// is skipped by [`ExternalAuthManager::rebuild`], so its sign-in would
/// never work), then the OIDC settings must pass
/// `oidc::validate_provider_config`.
pub fn validate_provider(
    provider_type: &str,
    issuer_url: Option<&str>,
    extra_config: Option<&serde_json::Value>,
    check_files: bool,
) -> Result<(), String> {
    if provider_type != SUPPORTED_PROVIDER_TYPE {
        return Err(format!(
            "provider_type '{provider_type}' is not supported: the only provider type is \
             '{SUPPORTED_PROVIDER_TYPE}' (Google, Okta and Azure AD are OIDC issuers)"
        ));
    }
    oidc::validate_provider_config(issuer_url, extra_config, check_files)
}

/// Discover `providers` at once; log each result. Returns how many succeeded.
async fn discover_providers(providers: Vec<Arc<OidcProvider>>) -> usize {
    let results = futures::future::join_all(providers.into_iter().map(|provider| async move {
        match provider.discover().await {
            Ok(()) => {
                tracing::info!("OIDC discovery completed for provider '{}'", provider.name);
                true
            }
            Err(e) => {
                tracing::warn!(
                    "OIDC discovery failed for provider '{}': {}",
                    provider.name,
                    e
                );
                false
            }
        }
    }))
    .await;
    results.into_iter().filter(|ok| *ok).count()
}

/// The error a login start answers for a provider that is not discovered.
/// Pure: `last` is the error of the provider's last discovery, `running`
/// tells whether the background task still retries.
/// - a non-transport error (HTTP 4xx, a refused URL, a bad document) is a
///   `DiscoveryFailed` with its cause: a retry would fail the same way;
/// - a transport error (timeout, refused connection, HTTP 5xx) is a
///   retryable `DiscoveryPending` only while the task runs; with no task,
///   nothing retries, so it is a `DiscoveryFailed`;
/// - no error and no task: nothing is discovering, so it is a failure too.
fn discovery_verdict(last: Option<ExternalAuthError>, running: bool) -> ExternalAuthError {
    match (last, running) {
        (Some(ExternalAuthError::DiscoveryUnreachable(why)), true) => {
            ExternalAuthError::DiscoveryPending(format!(
                "the proxy has not reached the identity provider yet ({why}); it keeps trying \
                 in the background. Try again in a few seconds."
            ))
        }
        (None, true) => ExternalAuthError::DiscoveryPending(
            "the proxy has not reached the identity provider yet; it keeps trying in the \
             background. Try again in a few seconds."
                .into(),
        ),
        (
            Some(
                ExternalAuthError::DiscoveryUnreachable(why)
                | ExternalAuthError::DiscoveryFailed(why),
            ),
            _,
        ) => ExternalAuthError::DiscoveryFailed(why),
        (Some(other), _) => ExternalAuthError::DiscoveryFailed(other.to_string()),
        (None, false) => ExternalAuthError::DiscoveryFailed(
            "the identity provider is not discovered and no discovery is running".into(),
        ),
    }
}

/// TTL for pending OAuth flows (5 minutes).
const PENDING_AUTH_TTL: Duration = Duration::from_secs(300);

/// Bound on pending OAuth flows. Every `GET /oauth/authorize/<provider>`
/// adds one, unauthenticated; without a bound, a client that sends them in
/// a loop grows the map for the whole TTL. Far above any real login rate.
const MAX_PENDING_AUTH: usize = 10_000;

/// Manages external authentication providers and pending OAuth flows.
pub struct ExternalAuthManager {
    /// Configured providers keyed by name.
    providers: RwLock<HashMap<String, Arc<OidcProvider>>>,
    /// Pending OAuth flows keyed by state token.
    pending: RwLock<HashMap<String, PendingAuth>>,
    /// Pending flows evicted because the map was full (log throttle).
    pending_evicted: std::sync::atomic::AtomicU64,
    /// True while the background discovery task runs (the single flight).
    discovery_running: std::sync::atomic::AtomicBool,
    /// Wakes the running task after a rebuild swapped the providers.
    discovery_kick: tokio::sync::Notify,
}

impl ExternalAuthManager {
    /// Create a new manager with no providers configured.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            pending_evicted: Default::default(),
            discovery_running: Default::default(),
            discovery_kick: Default::default(),
        }
    }

    /// Rebuild the provider map from ConfigDb records.
    /// Called at startup and when providers are modified via admin API.
    pub fn rebuild(&self, configs: &[AuthProviderConfig]) {
        let mut providers = HashMap::new();
        for config in configs {
            if !config.enabled {
                continue;
            }
            match config.provider_type.as_str() {
                "oidc" => {
                    if let (Some(client_id), Some(client_secret), Some(issuer_url)) = (
                        config.client_id.as_ref(),
                        config.client_secret.as_ref(),
                        config.issuer_url.as_ref(),
                    ) {
                        let provider = OidcProvider::new(
                            config.name.clone(),
                            client_id.clone(),
                            client_secret.clone(),
                            issuer_url.clone(),
                            config.scopes.clone(),
                            config.extra_config.clone().unwrap_or(serde_json::json!({})),
                        );
                        providers.insert(config.name.clone(), Arc::new(provider));
                    } else {
                        tracing::warn!(
                            "Skipping OIDC provider '{}': missing client_id, client_secret, or issuer_url",
                            config.name
                        );
                    }
                }
                other => {
                    tracing::warn!(
                        "Unsupported provider type '{}' for provider '{}' (only 'oidc' is currently supported)",
                        other,
                        config.name
                    );
                }
            }
        }
        *self.providers.write() = providers;
        // Invalidate all pending OAuth flows — they hold references to old provider configs
        // (client_id, secrets) that may no longer match. Users will need to restart the flow.
        let cleared = self.pending.write().len();
        self.pending.write().clear();
        if cleared > 0 {
            tracing::info!(
                "Cleared {} pending OAuth flow(s) after provider rebuild",
                cleared
            );
        }
    }

    /// The providers whose discovery is missing or older than the cache TTL.
    fn undiscovered(&self) -> Vec<Arc<OidcProvider>> {
        self.providers
            .read()
            .values()
            .filter(|p| !p.is_discovery_cached())
            .cloned()
            .collect()
    }

    /// True while the background discovery task runs.
    pub fn discovery_running(&self) -> bool {
        self.discovery_running
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Discover the providers in the BACKGROUND. EVERY caller that publishes
    /// a provider set (boot, config sync, admin edit, backup restore) calls
    /// this and never awaits a discovery: a hanging identity provider must
    /// not stall them. The server serves at once; until a provider is
    /// discovered, its login start answers a retryable `DiscoveryPending`
    /// (or `DiscoveryFailed` with the cause when a retry cannot help).
    ///
    /// Single flight: a call while the task runs starts no second task, it
    /// wakes the running one (the providers it must retry are the ones
    /// published now, read afresh each round). A call after the task ended
    /// starts a new one. The task retries with backoff (5 s, doubling to
    /// 5 min), ends when every provider is discovered or none is left, and
    /// bumps the version counter after a round that published new state.
    pub fn spawn_discovery(self: &Arc<Self>) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.discovery_running.swap(true, SeqCst) {
            self.discovery_kick.notify_one();
            return;
        }
        let manager = Arc::clone(self);
        crate::background::spawn_supervised("oidc-discovery", move || {
            let manager = Arc::clone(&manager);
            async move { manager.discovery_loop().await }
        });
    }

    async fn discovery_loop(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        const FIRST_WAIT: Duration = Duration::from_secs(5);
        let mut wait = FIRST_WAIT;
        loop {
            let pending = self.undiscovered();
            if pending.is_empty() {
                self.discovery_running.store(false, SeqCst);
                // A rebuild that raced the clear: its caller either saw
                // `false` and started a task (the swap below then returns
                // true), or saw `true` and kicked us (we take the flag back).
                if self.undiscovered().is_empty() || self.discovery_running.swap(true, SeqCst) {
                    return;
                }
                continue;
            }
            if discover_providers(pending).await > 0 {
                // After the discovered state is published.
                crate::api::admin::external_auth::bump_ext_auth_version();
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {
                    wait = (wait * 2).min(Duration::from_secs(300));
                }
                () = self.discovery_kick.notified() => wait = FIRST_WAIT,
            }
        }
    }

    /// Get a provider by name.
    pub fn get_provider(&self, name: &str) -> Option<Arc<OidcProvider>> {
        self.providers.read().get(name).cloned()
    }

    /// Get all provider names (for whoami response).
    pub fn provider_names(&self) -> Vec<String> {
        self.providers.read().keys().cloned().collect()
    }

    /// Check if any providers are configured.
    pub fn has_providers(&self) -> bool {
        !self.providers.read().is_empty()
    }

    /// Initiate an OAuth authorization flow.
    /// Generates state token, stores pending auth, returns the authorization URL.
    pub fn initiate_auth(
        &self,
        provider_name: &str,
        redirect_uri: &str,
        client_ip: Option<IpAddr>,
        next_url: Option<String>,
    ) -> Result<AuthorizationRequest, ExternalAuthError> {
        let provider = self
            .get_provider(provider_name)
            .ok_or_else(|| ExternalAuthError::ProviderNotFound(provider_name.to_string()))?;

        // Ensure discovery is cached
        if !provider.is_discovery_cached() {
            return Err(discovery_verdict(
                provider.last_discovery_error(),
                self.discovery_running(),
            ));
        }

        // Generate state token (32 bytes = 64 hex chars)
        let mut state_bytes = [0u8; 32];
        rand::rngs::OsRng.fill(&mut state_bytes);
        let state = hex::encode(state_bytes);

        let auth_req = provider.authorization_url(redirect_uri, &state)?;

        // Store pending auth for callback validation
        self.insert_pending(
            state.clone(),
            PendingAuth {
                provider_name: provider_name.to_string(),
                nonce: auth_req.nonce.clone(),
                pkce_verifier: auth_req.pkce_verifier.clone(),
                created_at: Instant::now(),
                client_ip,
                redirect_to: next_url,
            },
        );

        Ok(auth_req)
    }

    /// Store one pending flow, within [`MAX_PENDING_AUTH`]: a full map drops
    /// its expired flows, then its oldest one. The evicted user's login
    /// fails with `InvalidState` and can start again.
    fn insert_pending(&self, state: String, auth: PendingAuth) {
        let mut pending = self.pending.write();
        if pending.len() >= MAX_PENDING_AUTH {
            pending.retain(|_, a| a.created_at.elapsed() < PENDING_AUTH_TTL);
        }
        if pending.len() >= MAX_PENDING_AUTH {
            if let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, a)| a.created_at)
                .map(|(k, _)| k.clone())
            {
                pending.remove(&oldest);
            }
            let n = 1 + self
                .pending_evicted
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // Throttled: 1st, 2nd, 4th, 8th, ... eviction.
            if n.is_power_of_two() {
                tracing::warn!(
                    "SECURITY | event=oauth_pending_full | max={MAX_PENDING_AUTH} | evicted_total={n} — oldest pending OAuth logins dropped"
                );
            }
        }
        pending.insert(state, auth);
    }

    /// Validate and consume a pending OAuth flow by state token.
    /// Returns the PendingAuth if valid, removes it from the pending map.
    pub fn consume_pending(&self, state: &str) -> Result<PendingAuth, ExternalAuthError> {
        let mut pending = self.pending.write();
        match pending.remove(state) {
            Some(auth) if auth.created_at.elapsed() < PENDING_AUTH_TTL => Ok(auth),
            Some(_) => Err(ExternalAuthError::InvalidState),
            None => Err(ExternalAuthError::InvalidState),
        }
    }

    /// Clean up expired pending auth entries.
    pub fn cleanup_expired_pending(&self) {
        self.pending
            .write()
            .retain(|_, auth| auth.created_at.elapsed() < PENDING_AUTH_TTL);
    }
}

impl Default for ExternalAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_db::auth_providers::AuthProviderConfig;

    fn make_provider_config(name: &str, enabled: bool) -> AuthProviderConfig {
        AuthProviderConfig {
            id: 1,
            name: name.into(),
            provider_type: "oidc".into(),
            enabled,
            priority: 0,
            display_name: Some(name.into()),
            client_id: Some("cid".into()),
            client_secret: Some("csec".into()),
            issuer_url: Some("https://accounts.google.com".into()),
            scopes: "openid email".into(),
            extra_config: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn make_pending_auth(provider: &str, created_at: std::time::Instant) -> types::PendingAuth {
        types::PendingAuth {
            provider_name: provider.into(),
            nonce: None,
            pkce_verifier: None,
            created_at,
            client_ip: None,
            redirect_to: None,
        }
    }

    /// The login-start answer for a provider that is not discovered.
    #[test]
    fn the_login_start_verdict_table() {
        use ExternalAuthError::{
            DiscoveryFailed as F, DiscoveryPending as P, DiscoveryUnreachable as U,
        };
        let u = || Some(U("timed out".into()));
        let f = || Some(F("HTTP 404 Not Found from https://idp/x".into()));
        // (last error, task running) -> pending?
        let cases = [
            (u(), true, true),
            (None, true, true),
            // no task retries: a transport error is a failure the user must see
            (u(), false, false),
            (None, false, false),
            // a retry cannot cure it: always a failure, with the cause
            (f(), true, false),
            (f(), false, false),
        ];
        for (last, running, pending) in cases {
            let verdict = discovery_verdict(last, running);
            assert_eq!(
                matches!(verdict, P(_)),
                pending,
                "{verdict:?} running={running}"
            );
            assert!(matches!(verdict, P(_) | F(_)), "{verdict:?}");
        }
        match discovery_verdict(f(), true) {
            F(m) => assert!(m.contains("404"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// An identity provider that answers every request with 404 and counts
    /// the requests. Returns its issuer URL and the counter.
    fn not_found_idp() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (issuer, hits)
    }

    fn local_provider(name: &str, issuer: &str) -> AuthProviderConfig {
        let mut config = make_provider_config(name, true);
        config.issuer_url = Some(issuer.into());
        config.extra_config = Some(serde_json::json!({ "allow_local": true }));
        config
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..100 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out: {what}");
    }

    /// Single flight: calls while the task runs start no second task; the
    /// running task retries the providers a rebuild swapped in (not the
    /// stale ones); a call after the task ended starts a new task; a
    /// permanent error (HTTP 404) reaches the login start as a failure.
    #[tokio::test]
    async fn discovery_is_one_task_that_follows_the_rebuilt_providers() {
        use std::sync::atomic::Ordering::SeqCst;
        let (issuer_a, hits_a) = not_found_idp();
        let (issuer_b, hits_b) = not_found_idp();
        let mgr = Arc::new(ExternalAuthManager::new());

        mgr.rebuild(&[local_provider("corp", &issuer_a)]);
        for _ in 0..3 {
            mgr.spawn_discovery();
        }
        assert!(mgr.discovery_running());
        until("first round", || hits_a.load(SeqCst) >= 1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        // One task: round one, plus at most one round for the coalesced
        // wake-up. Three stacked tasks would send three requests at once.
        assert!(
            hits_a.load(SeqCst) <= 2,
            "stacked tasks: {}",
            hits_a.load(SeqCst)
        );
        // A 404 cannot be cured: the login start says so, with the cause.
        match mgr.initiate_auth("corp", "https://proxy/cb", None, None) {
            Err(ExternalAuthError::DiscoveryFailed(m)) => assert!(m.contains("404"), "{m}"),
            other => panic!("{other:?}"),
        }

        // A rebuild swaps the provider; the running task follows at once
        // (not after its backoff) and leaves the stale one alone.
        let stale = hits_a.load(SeqCst);
        mgr.rebuild(&[local_provider("corp", &issuer_b)]);
        mgr.spawn_discovery();
        until("the new provider is tried", || hits_b.load(SeqCst) >= 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hits_a.load(SeqCst), stale, "the stale provider was retried");

        // No provider left: the task ends, and a later call starts a new one.
        mgr.rebuild(&[]);
        mgr.spawn_discovery();
        until("the task ends", || !mgr.discovery_running()).await;
        let before = hits_b.load(SeqCst);
        mgr.rebuild(&[local_provider("corp", &issuer_b)]);
        mgr.spawn_discovery();
        assert!(mgr.discovery_running());
        until("a new task runs", || hits_b.load(SeqCst) > before).await;
    }

    /// A flood of authorize requests cannot grow the pending map past its
    /// bound; the oldest flow goes first, the newest survive.
    #[test]
    fn pending_auth_map_is_bounded_and_evicts_the_oldest() {
        let mgr = ExternalAuthManager::new();
        let t0 = std::time::Instant::now();
        for i in 0..MAX_PENDING_AUTH + 5 {
            mgr.insert_pending(
                format!("s{i}"),
                make_pending_auth("corp", t0 + Duration::from_millis(i as u64)),
            );
        }
        let pending = mgr.pending.read();
        assert_eq!(pending.len(), MAX_PENDING_AUTH);
        assert!(!pending.contains_key("s0") && !pending.contains_key("s4"));
        assert!(pending.contains_key("s5"));
        assert!(pending.contains_key(&format!("s{}", MAX_PENDING_AUTH + 4)));
    }

    #[test]
    fn test_new_manager_has_no_providers() {
        let mgr = ExternalAuthManager::new();
        assert!(!mgr.has_providers());
        assert!(mgr.provider_names().is_empty());
    }

    #[test]
    fn test_rebuild_with_enabled_provider() {
        let mgr = ExternalAuthManager::new();
        mgr.rebuild(&[make_provider_config("google", true)]);
        assert!(mgr.has_providers());
        assert_eq!(mgr.provider_names().len(), 1);
        assert!(mgr.get_provider("google").is_some());
    }

    #[test]
    fn test_rebuild_skips_disabled_providers() {
        let mgr = ExternalAuthManager::new();
        mgr.rebuild(&[make_provider_config("google", false)]);
        assert!(!mgr.has_providers());
        assert!(mgr.get_provider("google").is_none());
    }

    #[test]
    fn test_rebuild_skips_incomplete_providers() {
        let mgr = ExternalAuthManager::new();
        let mut config = make_provider_config("broken", true);
        config.client_id = None; // Missing required field
        mgr.rebuild(&[config]);
        assert!(!mgr.has_providers());
    }

    #[test]
    fn test_get_provider_unknown() {
        let mgr = ExternalAuthManager::new();
        assert!(mgr.get_provider("nonexistent").is_none());
    }

    #[test]
    fn test_consume_pending_removes_entry() {
        let mgr = ExternalAuthManager::new();
        // Manually insert a pending auth
        mgr.pending.write().insert(
            "state-123".into(),
            make_pending_auth("google", std::time::Instant::now()),
        );

        // Consume it
        let result = mgr.consume_pending("state-123");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().provider_name, "google");

        // Second consume fails
        let result = mgr.consume_pending("state-123");
        assert!(matches!(
            result,
            Err(types::ExternalAuthError::InvalidState)
        ));
    }

    #[test]
    fn test_consume_pending_expired() {
        let mgr = ExternalAuthManager::new();
        mgr.pending.write().insert(
            "state-old".into(),
            make_pending_auth(
                "google",
                std::time::Instant::now() - PENDING_AUTH_TTL - Duration::from_secs(1),
            ),
        );

        let result = mgr.consume_pending("state-old");
        assert!(matches!(
            result,
            Err(types::ExternalAuthError::InvalidState)
        ));
    }

    #[test]
    fn test_cleanup_expired_pending() {
        let mgr = ExternalAuthManager::new();

        // Fresh entry
        mgr.pending.write().insert(
            "fresh".into(),
            make_pending_auth("google", std::time::Instant::now()),
        );
        // Expired entry
        mgr.pending.write().insert(
            "expired".into(),
            make_pending_auth(
                "google",
                std::time::Instant::now() - PENDING_AUTH_TTL - Duration::from_secs(1),
            ),
        );

        assert_eq!(mgr.pending.read().len(), 2);
        mgr.cleanup_expired_pending();
        assert_eq!(mgr.pending.read().len(), 1);
        assert!(mgr.pending.read().contains_key("fresh"));
    }

    #[test]
    fn test_consume_pending_unknown_state() {
        let mgr = ExternalAuthManager::new();
        let result = mgr.consume_pending("nonexistent");
        assert!(matches!(
            result,
            Err(types::ExternalAuthError::InvalidState)
        ));
    }
}
