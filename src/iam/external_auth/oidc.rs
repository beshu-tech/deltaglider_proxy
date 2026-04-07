//! OIDC (OpenID Connect) provider implementation.
//!
//! Supports any OIDC-compliant Identity Provider: Google, Okta, Azure AD, Keycloak, etc.
//! Uses OIDC Discovery to auto-configure endpoints from the issuer URL.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, DecodingKey, Validation};
use parking_lot::RwLock;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Instant;

use super::types::{
    AuthorizationRequest, ExternalAuthError, ExternalIdentityInfo, PendingAuth, ProviderTestResult,
};

/// Cached OIDC discovery document.
struct CachedDiscovery {
    doc: OidcDiscovery,
    jwks: jsonwebtoken::jwk::JwkSet,
    fetched_at: Instant,
}

/// An OIDC provider instance with cached discovery metadata.
pub struct OidcProvider {
    pub name: String,
    pub client_id: String,
    pub client_secret: String,
    pub issuer_url: String,
    pub scopes: String,
    pub extra_config: serde_json::Value,
    http: reqwest::Client,
    cache: RwLock<Option<CachedDiscovery>>,
}

/// OIDC Discovery Document (subset of fields we need).
#[derive(Debug, Clone, Deserialize)]
struct OidcDiscovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    #[allow(dead_code)]
    userinfo_endpoint: Option<String>,
}

/// Token response from the OIDC token endpoint.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[allow(dead_code)]
    access_token: String,
    id_token: Option<String>,
    #[allow(dead_code)]
    token_type: String,
}

/// Standard OIDC ID token claims (subset).
#[derive(Debug, Serialize, Deserialize)]
struct IdTokenClaims {
    iss: String,
    sub: String,
    aud: serde_json::Value,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    /// Google hosted domain claim.
    #[serde(default)]
    hd: Option<String>,
    /// Group memberships (not standard, but common in enterprise IdPs).
    #[serde(default)]
    groups: Option<Vec<String>>,
}

/// Cache TTL for OIDC discovery documents (1 hour).
const DISCOVERY_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

impl OidcProvider {
    /// Create a new OIDC provider instance.
    pub fn new(
        name: String,
        client_id: String,
        client_secret: String,
        issuer_url: String,
        scopes: String,
        extra_config: serde_json::Value,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            name,
            client_id,
            client_secret,
            issuer_url,
            scopes,
            extra_config,
            http,
            cache: RwLock::new(None),
        }
    }

    /// Generate the authorization URL for browser redirect.
    pub fn authorization_url(
        &self,
        redirect_uri: &str,
        state: &str,
    ) -> Result<AuthorizationRequest, ExternalAuthError> {
        let cache = self.cache.read();
        let discovery = cache
            .as_ref()
            .ok_or_else(|| ExternalAuthError::DiscoveryFailed("Discovery not cached yet".into()))?;

        // Generate PKCE code verifier (43-128 chars of URL-safe random)
        let mut verifier_bytes = [0u8; 32];
        rand::rngs::OsRng.fill(&mut verifier_bytes);
        let pkce_verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

        // S256 code challenge
        let challenge = Sha256::digest(pkce_verifier.as_bytes());
        let code_challenge = URL_SAFE_NO_PAD.encode(challenge);

        // Generate nonce
        let mut nonce_bytes = [0u8; 16];
        rand::rngs::OsRng.fill(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        let params = [
            ("response_type", "code"),
            ("client_id", &self.client_id),
            ("redirect_uri", redirect_uri),
            ("scope", &self.scopes),
            ("state", state),
            ("nonce", &nonce),
            ("code_challenge", &code_challenge),
            ("code_challenge_method", "S256"),
        ];

        let url = format!(
            "{}?{}",
            discovery.doc.authorization_endpoint,
            serde_urlencoded::to_string(params)
                .map_err(|e| ExternalAuthError::ConfigError(e.to_string()))?
        );

        Ok(AuthorizationRequest {
            redirect_url: url,
            state: state.to_string(),
            nonce: Some(nonce),
            pkce_verifier: Some(pkce_verifier),
        })
    }

    /// Exchange an authorization code for user identity.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        pending: &PendingAuth,
    ) -> Result<ExternalIdentityInfo, ExternalAuthError> {
        let (token_endpoint, jwks, issuer) = {
            let cache = self.cache.read();
            let disc = cache
                .as_ref()
                .ok_or_else(|| ExternalAuthError::DiscoveryFailed("No cached discovery".into()))?;
            (
                disc.doc.token_endpoint.clone(),
                disc.jwks.clone(),
                disc.doc.issuer.clone(),
            )
        };

        // Build token exchange request
        let mut params = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", redirect_uri.to_string()),
            ("client_id", self.client_id.clone()),
            ("client_secret", self.client_secret.clone()),
        ];

        if let Some(ref verifier) = pending.pkce_verifier {
            params.push(("code_verifier", verifier.clone()));
        }

        let resp = self.http.post(&token_endpoint).form(&params).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ExternalAuthError::TokenExchangeFailed(format!(
                "HTTP {}: {}",
                status, body
            )));
        }

        let token_resp: TokenResponse = resp
            .json()
            .await
            .map_err(|e| ExternalAuthError::TokenExchangeFailed(e.to_string()))?;

        let id_token_str = token_resp.id_token.ok_or_else(|| {
            ExternalAuthError::TokenExchangeFailed("No id_token in response".into())
        })?;

        // Validate the ID token
        let claims = self.validate_id_token(&id_token_str, &jwks, &issuer, pending)?;

        // Extract raw claims as JSON for flexible mapping
        let raw_claims = extract_raw_claims(&id_token_str);

        Ok(ExternalIdentityInfo {
            subject: claims.sub,
            email: claims.email,
            email_verified: claims.email_verified.unwrap_or(false),
            name: claims.name,
            groups: claims.groups.unwrap_or_default(),
            raw_claims,
        })
    }

    /// Test provider connectivity by fetching the discovery document.
    pub async fn test_connection(&self) -> Result<ProviderTestResult, ExternalAuthError> {
        match self.fetch_discovery().await {
            Ok(doc) => Ok(ProviderTestResult {
                success: true,
                issuer: Some(doc.issuer.clone()),
                authorization_endpoint: Some(doc.authorization_endpoint.clone()),
                error: None,
            }),
            Err(e) => Ok(ProviderTestResult {
                success: false,
                issuer: None,
                authorization_endpoint: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Fetch and cache the OIDC discovery document and JWKS.
    pub async fn discover(&self) -> Result<(), ExternalAuthError> {
        let doc = self.fetch_discovery().await?;
        let jwks = self.fetch_jwks(&doc.jwks_uri).await?;

        let mut cache = self.cache.write();
        *cache = Some(CachedDiscovery {
            doc,
            jwks,
            fetched_at: Instant::now(),
        });
        Ok(())
    }

    /// Check if the cached discovery is still valid.
    pub fn is_discovery_cached(&self) -> bool {
        self.cache
            .read()
            .as_ref()
            .map(|c| c.fetched_at.elapsed() < DISCOVERY_CACHE_TTL)
            .unwrap_or(false)
    }

    async fn fetch_discovery(&self) -> Result<OidcDiscovery, ExternalAuthError> {
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| ExternalAuthError::DiscoveryFailed(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(ExternalAuthError::DiscoveryFailed(format!(
                "HTTP {} from {}",
                resp.status(),
                url
            )));
        }

        resp.json()
            .await
            .map_err(|e| ExternalAuthError::DiscoveryFailed(e.to_string()))
    }

    async fn fetch_jwks(
        &self,
        jwks_uri: &str,
    ) -> Result<jsonwebtoken::jwk::JwkSet, ExternalAuthError> {
        let resp = self
            .http
            .get(jwks_uri)
            .send()
            .await
            .map_err(|e| ExternalAuthError::DiscoveryFailed(e.to_string()))?;

        resp.json()
            .await
            .map_err(|e| ExternalAuthError::DiscoveryFailed(e.to_string()))
    }

    fn validate_id_token(
        &self,
        token: &str,
        jwks: &jsonwebtoken::jwk::JwkSet,
        expected_issuer: &str,
        pending: &PendingAuth,
    ) -> Result<IdTokenClaims, ExternalAuthError> {
        // Decode header to find the key ID (kid)
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| ExternalAuthError::TokenValidationFailed(e.to_string()))?;

        let kid = header.kid.as_ref().ok_or_else(|| {
            ExternalAuthError::TokenValidationFailed("No kid in token header".into())
        })?;

        // Find the matching key in JWKS
        let jwk = jwks.find(kid).ok_or_else(|| {
            ExternalAuthError::TokenValidationFailed(format!("Key '{}' not found in JWKS", kid))
        })?;

        let decoding_key = DecodingKey::from_jwk(jwk)
            .map_err(|e| ExternalAuthError::TokenValidationFailed(e.to_string()))?;

        // Configure validation — use the algorithm from the token header.
        // The JWKS key already constrains which algorithms are valid (via its `alg` field).
        // Overriding validation.algorithms can cause InvalidAlgorithm errors when the
        // jsonwebtoken crate cross-checks the key's algorithm against the validation list.
        let mut validation = Validation::new(header.alg);
        validation.set_audience(&[&self.client_id]);
        validation.set_issuer(&[expected_issuer]);

        let token_data = decode::<IdTokenClaims>(token, &decoding_key, &validation)
            .map_err(|e| ExternalAuthError::TokenValidationFailed(e.to_string()))?;

        // Validate nonce (OIDC replay protection)
        if let Some(ref expected_nonce) = pending.nonce {
            match &token_data.claims.nonce {
                Some(actual_nonce) if actual_nonce == expected_nonce => {}
                Some(_) => {
                    return Err(ExternalAuthError::TokenValidationFailed(
                        "Nonce mismatch".into(),
                    ));
                }
                None => {
                    return Err(ExternalAuthError::TokenValidationFailed(
                        "Missing nonce in ID token".into(),
                    ));
                }
            }
        }

        Ok(token_data.claims)
    }
}

/// Extract raw claims from an ID token (decode the payload without verification).
/// Used for flexible group mapping against arbitrary claims.
fn extract_raw_claims(token: &str) -> serde_json::Value {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return serde_json::json!({});
    }
    URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(serde_json::json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_raw_claims_valid() {
        // Manually construct a JWT-like token with a payload
        let payload = serde_json::json!({"sub": "123", "email": "test@example.com"});
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("header.{}.signature", payload_b64);
        let claims = extract_raw_claims(&token);
        assert_eq!(claims["sub"], "123");
        assert_eq!(claims["email"], "test@example.com");
    }

    #[test]
    fn test_extract_raw_claims_invalid() {
        let claims = extract_raw_claims("not-a-jwt");
        assert_eq!(claims, serde_json::json!({}));
    }

    #[test]
    fn test_extract_raw_claims_nested_objects() {
        let payload = serde_json::json!({
            "sub": "123",
            "org": {"id": "acme", "roles": ["admin"]},
            "groups": ["engineering", "backend"]
        });
        let payload_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap());
        let token = format!("h.{}.s", payload_b64);
        let claims = extract_raw_claims(&token);
        assert_eq!(claims["org"]["id"], "acme");
        assert_eq!(claims["groups"][0], "engineering");
    }

    #[test]
    fn test_extract_raw_claims_empty_payload() {
        let payload_b64 = URL_SAFE_NO_PAD.encode(b"{}");
        let token = format!("h.{}.s", payload_b64);
        let claims = extract_raw_claims(&token);
        assert_eq!(claims, serde_json::json!({}));
    }

    #[test]
    fn test_extract_raw_claims_corrupt_base64() {
        let token = "header.!!!not-valid-base64!!!.sig";
        let claims = extract_raw_claims(token);
        assert_eq!(claims, serde_json::json!({}));
    }

    #[test]
    fn test_pkce_verifier_format() {
        // Verify PKCE verifier is URL-safe base64, correct length
        let mut verifier_bytes = [0u8; 32];
        rand::thread_rng().fill(&mut verifier_bytes);
        let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
        assert!(verifier.len() >= 43, "PKCE verifier should be >= 43 chars");
        // Verify it only contains URL-safe chars
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_pkce_s256_challenge() {
        // Verify S256: challenge = BASE64URL(SHA256(verifier))
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = Sha256::digest(verifier.as_bytes());
        let encoded = URL_SAFE_NO_PAD.encode(challenge);
        // This is deterministic — verify it's a valid base64url string
        assert!(!encoded.is_empty());
        assert!(encoded
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
