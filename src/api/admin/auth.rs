//! Auth handlers: login, logout, login_as, whoami, check_session, require_session.

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use subtle::ConstantTimeEq;

use crate::iam::IamState;
use crate::rate_limiter;

use super::{audit_log, AdminState};

#[derive(Deserialize)]
pub struct LoginRequest {
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    ok: bool,
}

#[derive(Serialize)]
pub struct SessionResponse {
    valid: bool,
}

#[derive(Deserialize)]
pub struct WhoamiQuery {
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
}

#[derive(Serialize)]
pub struct WhoamiUser {
    name: String,
    access_key_id: String,
    is_admin: bool,
}

#[derive(Serialize)]
pub struct WhoamiResponse {
    mode: String,
    user: Option<WhoamiUser>,
}

#[derive(Deserialize)]
pub struct LoginAsRequest {
    access_key_id: String,
    secret_access_key: String,
}

/// Format a session cookie for setting a login token.
/// Max-Age matches session TTL (default 4h = 14400s, overridable via DGP_SESSION_TTL_HOURS).
pub(super) fn session_cookie(token: &str) -> String {
    let ttl_hours: u64 = std::env::var("DGP_SESSION_TTL_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let max_age = ttl_hours * 3600;
    format!(
        "dgp_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}",
        token, max_age
    )
}

/// Format a session cookie that clears the login token.
pub(super) fn session_cookie_clear() -> &'static str {
    "dgp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
}

/// Extract the `dgp_session` token from the Cookie header.
pub(super) fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            part.strip_prefix("dgp_session=")
                .map(|value| value.to_string())
        })
}

/// POST /api/admin/login — verify password, set session cookie.
pub async fn login(
    State(state): State<Arc<AdminState>>,
    req_headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // Extract client IP for rate limiting
    let client_ip = rate_limiter::extract_client_ip(&req_headers)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

    // Check rate limit before processing
    if state.rate_limiter.is_limited(&client_ip) {
        tracing::warn!("Rate limited login attempt from {}", client_ip);
        return (
            StatusCode::TOO_MANY_REQUESTS,
            HeaderMap::new(),
            Json(LoginResponse { ok: false }),
        )
            .into_response();
    }

    let hash = state.password_hash.read().clone();
    let valid = match bcrypt::verify(&body.password, &hash) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bcrypt verify failed (corrupted hash?): {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                Json(LoginResponse { ok: false }),
            )
                .into_response();
        }
    };

    if !valid {
        let locked = state.rate_limiter.record_failure(&client_ip);
        tracing::warn!(
            "Failed login attempt from {} (locked={})",
            client_ip,
            locked
        );
        audit_log("login_failed", "", "bootstrap", &req_headers);
        return (
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Json(LoginResponse { ok: false }),
        )
            .into_response();
    }

    // Successful login — reset rate limiter for this IP
    state.rate_limiter.record_success(&client_ip);
    let token = state
        .sessions
        .create_session(rate_limiter::extract_client_ip(&req_headers));

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, session_cookie(&token).parse().unwrap());

    (StatusCode::OK, headers, Json(LoginResponse { ok: true })).into_response()
}

/// POST /api/admin/logout — clear session.
pub async fn logout(State(state): State<Arc<AdminState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        state.sessions.remove(&token);
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::SET_COOKIE, session_cookie_clear().parse().unwrap());

    (
        StatusCode::OK,
        resp_headers,
        Json(LoginResponse { ok: true }),
    )
}

/// GET /api/admin/session — check if current session is valid.
pub async fn check_session(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let client_ip = rate_limiter::extract_client_ip(&headers);
    let valid = extract_session_token(&headers)
        .map(|t| state.sessions.validate(&t, client_ip))
        .unwrap_or(false);

    Json(SessionResponse { valid })
}

/// GET /api/whoami — returns current auth mode and user info for a given access key.
/// Public endpoint — no session required. Only reveals mode and whether the
/// access key exists (not names or admin status, to prevent enumeration attacks).
/// The is_admin field requires the secret_access_key as proof of identity.
pub async fn whoami(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(params): axum::extract::Query<WhoamiQuery>,
) -> Json<WhoamiResponse> {
    let iam_state = state.iam_state.load();
    match &**iam_state {
        IamState::Disabled => Json(WhoamiResponse {
            mode: "open".into(),
            user: None,
        }),
        IamState::Legacy(_) => Json(WhoamiResponse {
            mode: "bootstrap".into(),
            user: None,
        }),
        IamState::Iam(index) => {
            // Only reveal user info if both access_key_id AND secret_access_key match.
            // This prevents enumeration attacks (attacker can't probe access keys
            // without knowing the secret).
            let user = params
                .access_key_id
                .as_deref()
                .and_then(|ak| index.get(ak))
                .filter(|u| {
                    params
                        .secret_access_key
                        .as_deref()
                        .map(|sk| bool::from(sk.as_bytes().ct_eq(u.secret_access_key.as_bytes())))
                        .unwrap_or(false)
                })
                .map(|u| WhoamiUser {
                    name: u.name.clone(),
                    access_key_id: u.access_key_id.clone(),
                    is_admin: u.is_admin(),
                });
            Json(WhoamiResponse {
                mode: "iam".into(),
                user,
            })
        }
    }
}

/// POST /api/admin/login-as — create admin session for an IAM user with admin permissions.
/// Requires both access_key_id AND secret_access_key for authentication.
pub async fn login_as(
    State(state): State<Arc<AdminState>>,
    req_headers: HeaderMap,
    Json(body): Json<LoginAsRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Extract client IP for rate limiting
    let client_ip = rate_limiter::extract_client_ip(&req_headers)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

    // Check rate limit before processing
    if state.rate_limiter.is_limited(&client_ip) {
        tracing::warn!("Rate limited login-as attempt from {}", client_ip);
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    let iam_state = state.iam_state.load();
    let user = match &**iam_state {
        IamState::Iam(index) => index.get(&body.access_key_id),
        _ => None,
    };

    let user = match user {
        Some(u) => u,
        None => {
            state.rate_limiter.record_failure(&client_ip);
            tracing::warn!(
                "Failed login-as attempt from {} (unknown access key '{}')",
                client_ip,
                body.access_key_id
            );
            audit_log("login_failed", "", &body.access_key_id, &req_headers);
            return Err(StatusCode::FORBIDDEN);
        }
    };

    // Verify the secret key matches (critical — prevents auth bypass)
    // Use constant-time comparison to prevent timing side-channel attacks.
    if user
        .secret_access_key
        .as_bytes()
        .ct_ne(body.secret_access_key.as_bytes())
        .into()
    {
        state.rate_limiter.record_failure(&client_ip);
        tracing::warn!(
            "Failed login-as attempt from {} (secret mismatch for '{}')",
            client_ip,
            body.access_key_id
        );
        audit_log("login_failed", "", &body.access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN);
    }

    if !user.enabled || !user.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }

    // Successful login — reset rate limiter
    state.rate_limiter.record_success(&client_ip);

    let token = state
        .sessions
        .create_session(rate_limiter::extract_client_ip(&req_headers));
    tracing::info!(
        "Admin session created via login-as for '{}' ({})",
        user.name,
        user.access_key_id
    );

    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, session_cookie(&token))],
        Json(LoginResponse { ok: true }),
    ))
}

/// Middleware: validate session for protected admin routes.
/// Returns 401 if the session cookie is missing or invalid.
pub async fn require_session(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let client_ip = rate_limiter::extract_client_ip(&headers);
    let valid = extract_session_token(&headers)
        .map(|t| state.sessions.validate(&t, client_ip))
        .unwrap_or(false);

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    next.run(request).await.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iam::{AuthConfig, SharedIamState};
    use arc_swap::ArcSwap;

    /// Regression: SharedAuthConfig must reflect credential updates immediately.
    /// This guards against reverting to a static Extension<Option<Arc<AuthConfig>>>.
    #[test]
    fn shared_auth_config_reflects_updates() {
        let shared: SharedIamState = Arc::new(ArcSwap::from_pointee(IamState::Disabled));

        // Initially no auth
        assert!(matches!(&**shared.load(), IamState::Disabled));

        // Simulate admin API updating credentials
        shared.store(Arc::new(IamState::Legacy(AuthConfig {
            access_key_id: "new-key".to_string(),
            secret_access_key: "new-secret".to_string(),
        })));

        // Middleware must see the update
        let loaded = shared.load();
        match &**loaded {
            IamState::Legacy(auth) => {
                assert_eq!(auth.access_key_id, "new-key");
                assert_eq!(auth.secret_access_key, "new-secret");
            }
            _ => panic!("Expected IamState::Legacy"),
        }

        // Simulate disabling auth (clearing both credentials)
        shared.store(Arc::new(IamState::Disabled));
        assert!(matches!(&**shared.load(), IamState::Disabled));
    }

    #[test]
    fn extract_session_token_from_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "dgp_session=abc123".parse().unwrap());
        assert_eq!(extract_session_token(&headers).unwrap(), "abc123");

        // Multiple cookies
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "foo=bar; dgp_session=xyz789; baz=qux".parse().unwrap(),
        );
        assert_eq!(extract_session_token(&headers).unwrap(), "xyz789");

        // No session cookie
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "foo=bar".parse().unwrap());
        assert!(extract_session_token(&headers).is_none());

        // No cookie header at all
        assert!(extract_session_token(&HeaderMap::new()).is_none());
    }
}
