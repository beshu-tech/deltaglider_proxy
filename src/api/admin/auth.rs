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
use crate::session::S3SessionCredentials;

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

/// Query parameters for the whoami endpoint.
/// Fields are accepted but ignored — kept for backward compatibility with
/// older frontends that may still send `access_key_id`.
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct WhoamiQuery {
    access_key_id: Option<String>,
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

/// Whether session cookies should include the `Secure` flag (HTTPS-only).
/// Controlled by `DGP_SECURE_COOKIES`. Defaults to `true` — set to `false`
/// only for local development over plain HTTP.
fn secure_cookies() -> bool {
    std::env::var("DGP_SECURE_COOKIES")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true)
}

/// Format a session cookie for setting a login token.
/// Max-Age matches the session store's TTL.
pub(super) fn session_cookie(token: &str, ttl: std::time::Duration) -> String {
    let max_age = ttl.as_secs();
    let secure = if secure_cookies() { "; Secure" } else { "" };
    format!(
        "dgp_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
        token, max_age, secure
    )
}

/// Format a session cookie that clears the login token.
pub(super) fn session_cookie_clear() -> String {
    let secure = if secure_cookies() { "; Secure" } else { "" };
    format!(
        "dgp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        secure
    )
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
        let count = state.rate_limiter.failure_count(&client_ip);
        tracing::warn!(
            "SECURITY | event=admin_brute_force_blocked | ip={} | attempts={}",
            client_ip,
            count
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            HeaderMap::new(),
            Json(LoginResponse { ok: false }),
        )
            .into_response();
    }
    // Progressive delay: slow down responses under brute force
    let delay = state.rate_limiter.progressive_delay(&client_ip);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
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
        let count = state.rate_limiter.failure_count(&client_ip);
        if locked {
            tracing::warn!(
                "SECURITY | event=admin_brute_force_lockout | ip={} | attempts={}",
                client_ip,
                count
            );
        }
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

    // Auto-populate S3 credentials from config so "login IS connect".
    // The legacy access_key_id/secret_access_key are the proxy's own auth credentials.
    {
        let config = state.config.read().await;
        if let (Some(ak), Some(sk)) = (&config.access_key_id, &config.secret_access_key) {
            let region = match &config.backend {
                crate::config::BackendConfig::S3 { region, .. } => region.clone(),
                _ => "us-east-1".to_string(),
            };
            state.sessions.set_s3_creds(
                &token,
                S3SessionCredentials {
                    endpoint: String::new(),
                    region,
                    bucket: String::new(),
                    access_key_id: ak.clone(),
                    secret_access_key: sk.clone(),
                },
            );
        }
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        session_cookie(&token, state.sessions.ttl())
            .parse()
            .unwrap(),
    );

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

/// GET /api/whoami — returns current auth mode.
/// Public endpoint — no session required. Only reveals the auth mode
/// ("open", "bootstrap", "iam") — never user details, to prevent enumeration.
/// User identity is established via login / login-as (POST), not this endpoint.
pub async fn whoami(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(_params): axum::extract::Query<WhoamiQuery>,
) -> Json<WhoamiResponse> {
    let iam_state = state.iam_state.load();
    let mode = match &**iam_state {
        IamState::Disabled => "open",
        IamState::Legacy(_) => "bootstrap",
        IamState::Iam(_) => "iam",
    };
    Json(WhoamiResponse {
        mode: mode.into(),
        user: None,
    })
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
        let count = state.rate_limiter.failure_count(&client_ip);
        tracing::warn!(
            "SECURITY | event=login_as_brute_force_blocked | ip={} | attempts={}",
            client_ip,
            count
        );
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    // Progressive delay under brute force
    let delay = state.rate_limiter.progressive_delay(&client_ip);
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
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

    // Auto-populate S3 credentials from the IAM login so "login IS connect"
    {
        let config = state.config.read().await;
        let region = match &config.backend {
            crate::config::BackendConfig::S3 { region, .. } => region.clone(),
            _ => "us-east-1".to_string(),
        };
        state.sessions.set_s3_creds(
            &token,
            S3SessionCredentials {
                endpoint: String::new(),
                region,
                bucket: String::new(),
                access_key_id: body.access_key_id.clone(),
                secret_access_key: body.secret_access_key.clone(),
            },
        );
    }

    tracing::info!(
        "Admin session created via login-as for '{}' ({})",
        user.name,
        user.access_key_id
    );

    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            session_cookie(&token, state.sessions.ttl()),
        )],
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

// ── S3 Session Credentials ──

/// GET /api/admin/session/s3-credentials — retrieve stored S3 credentials.
/// Returns 404 if no credentials are stored in this session.
pub async fn get_s3_session_creds(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = match extract_session_token(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    match state.sessions.get_s3_creds(&token) {
        Some(creds) => (StatusCode::OK, Json(creds)).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// PUT /api/admin/session/s3-credentials — store or update S3 credentials.
/// Used by the ConnectPage when connecting to a custom endpoint.
pub async fn set_s3_session_creds(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(creds): Json<S3SessionCredentials>,
) -> impl IntoResponse {
    let token = match extract_session_token(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    state.sessions.set_s3_creds(&token, creds);
    StatusCode::OK.into_response()
}

/// DELETE /api/admin/session/s3-credentials — clear S3 credentials (disconnect).
pub async fn clear_s3_session_creds(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = match extract_session_token(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    state.sessions.clear_s3_creds(&token);
    StatusCode::OK.into_response()
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
