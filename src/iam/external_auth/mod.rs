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

/// TTL for pending OAuth flows (5 minutes).
const PENDING_AUTH_TTL: Duration = Duration::from_secs(300);

/// Manages external authentication providers and pending OAuth flows.
pub struct ExternalAuthManager {
    /// Configured providers keyed by name.
    providers: RwLock<HashMap<String, Arc<OidcProvider>>>,
    /// Pending OAuth flows keyed by state token.
    pending: RwLock<HashMap<String, PendingAuth>>,
}

impl ExternalAuthManager {
    /// Create a new manager with no providers configured.
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
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
    }

    /// Run OIDC discovery for all providers (fetch endpoints and JWKS).
    /// Called at startup and after provider changes.
    pub async fn discover_all(&self) {
        let providers: Vec<Arc<OidcProvider>> =
            { self.providers.read().values().cloned().collect() };
        for provider in providers {
            if let Err(e) = provider.discover().await {
                tracing::warn!(
                    "OIDC discovery failed for provider '{}': {}",
                    provider.name,
                    e
                );
            } else {
                tracing::info!("OIDC discovery completed for provider '{}'", provider.name);
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
            return Err(ExternalAuthError::DiscoveryFailed(
                "Discovery not cached. Try again shortly.".into(),
            ));
        }

        // Generate state token (32 bytes = 64 hex chars)
        let mut state_bytes = [0u8; 32];
        rand::rngs::OsRng.fill(&mut state_bytes);
        let state = hex::encode(state_bytes);

        let auth_req = provider.authorization_url(redirect_uri, &state)?;

        // Store pending auth for callback validation
        self.pending.write().insert(
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
