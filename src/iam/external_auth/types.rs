// SPDX-License-Identifier: BUSL-1.1

//! Shared types for external authentication providers.

use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::time::Instant;

/// Information about an external identity returned after successful authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalIdentityInfo {
    /// Stable, unique subject identifier from the provider (OIDC "sub" claim).
    pub subject: String,
    /// User's email address (from "email" claim).
    pub email: Option<String>,
    /// Whether the email has been verified by the provider.
    pub email_verified: bool,
    /// Display name (from "name" claim).
    pub name: Option<String>,
    /// Group memberships from the provider (from "groups" claim, if available).
    pub groups: Vec<String>,
    /// Full claims object for flexible mapping rules.
    pub raw_claims: serde_json::Value,
}

/// Result of initiating an authorization flow.
#[derive(Debug)]
pub struct AuthorizationRequest {
    /// Full URL to redirect the user's browser to.
    pub redirect_url: String,
    /// State parameter for CSRF protection (stored server-side).
    pub state: String,
    /// OIDC nonce for replay protection.
    pub nonce: Option<String>,
    /// PKCE code verifier (stored server-side, never sent to browser).
    pub pkce_verifier: Option<String>,
}

/// Server-side state for a pending OAuth flow.
#[derive(Debug)]
pub struct PendingAuth {
    /// Name of the provider this flow is for.
    pub provider_name: String,
    /// OIDC nonce (validated in the ID token).
    pub nonce: Option<String>,
    /// PKCE code verifier (used in the token exchange).
    pub pkce_verifier: Option<String>,
    /// When this flow was initiated (for TTL enforcement).
    pub created_at: Instant,
    /// Client IP that initiated the flow (for session binding).
    pub client_ip: Option<IpAddr>,
    /// Where to redirect after successful auth (from `next` query param).
    pub redirect_to: Option<String>,
}

/// Result of testing a provider's connectivity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderTestResult {
    pub success: bool,
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub error: Option<String>,
}

/// Errors from external authentication operations.
#[derive(Debug, thiserror::Error)]
pub enum ExternalAuthError {
    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("OAuth state invalid or expired")]
    InvalidState,

    #[error("OIDC discovery failed: {0}")]
    DiscoveryFailed(String),

    /// The identity provider did not answer (timeout, refused connection,
    /// DNS, HTTP 5xx or 429). A retry can succeed. Shown as a failure
    /// (`DiscoveryFailed`) once nothing retries it.
    #[error("OIDC discovery failed: {0}")]
    DiscoveryUnreachable(String),

    /// The provider's discovery has not finished yet (the proxy discovers in
    /// the background after a start, and retries with backoff). Retryable.
    #[error("OIDC discovery is not finished yet: {0}")]
    DiscoveryPending(String),

    #[error("Token exchange failed: {0}")]
    TokenExchangeFailed(String),

    #[error("ID token validation failed: {0}")]
    TokenValidationFailed(String),

    /// The token names a signing key (`kid`) that the cached JWKS lacks: the
    /// provider may have rotated its keys. One rediscovery can cure it.
    /// Its own variant, so the retry never matches on message text.
    #[error("ID token validation failed: Key '{0}' not found in JWKS")]
    UnknownSigningKey(String),

    #[error("Email not verified by provider")]
    EmailNotVerified,

    #[error("User account is disabled")]
    UserDisabled,

    #[error("HTTP request failed: {0}")]
    HttpError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),
}

impl From<reqwest::Error> for ExternalAuthError {
    fn from(e: reqwest::Error) -> Self {
        Self::HttpError(error_chain(&e))
    }
}

/// An error and every `source()` below it, joined with ": ". reqwest's own
/// Display stops at "error sending request for url (...)"; the cause (DNS,
/// refused connection, the TLS verdict such as `UnknownIssuer`) is in the
/// source chain, and an operator needs it.
pub fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(src) = cur {
        let s = src.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = src.source();
    }
    out
}
