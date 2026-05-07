//! Auth handlers: login, logout, login_as, whoami, check_session, require_session.

use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use subtle::ConstantTimeEq;

use crate::iam::{Group, IamIndex, IamState, IamUser};
use crate::rate_limiter;
use crate::session::{AuthMethod, S3SessionCredentials, SessionKind};

use super::{audit_log, AdminState};

/// Marker type inserted by [`require_admin_gui_session`] into request extensions.
/// Handlers that must never run under a browser-lift session take
/// `Extension<AdminGuiGate>` so a mistaken route merge fails at compile time
/// (extractor present) and is double-checked at runtime (missing extension → 500).
#[derive(Clone, Copy, Debug)]
pub struct AdminGuiGate;

/// Constant-time secret check + `enabled` gate for IAM index users.
/// Shared by `login-as`, `browser-session-connect`, and `POST /api/iam/identity`.
pub(crate) fn iam_user_secret_valid(user: &IamUser, secret_access_key: &str) -> bool {
    if !user.enabled {
        return false;
    }
    use sha2::{Digest, Sha256};
    let stored_hash = Sha256::digest(user.secret_access_key.as_bytes());
    let provided_hash = Sha256::digest(secret_access_key.as_bytes());
    stored_hash.ct_eq(&provided_hash).into()
}

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
    /// Full admin GUI session (config, usage scanner, etc.) — false for S3BrowserLift-only cookies.
    #[serde(default)]
    admin_gui: bool,
}

#[derive(Serialize)]
pub struct WhoamiUserInfo {
    pub name: String,
    pub access_key_id: String,
    pub is_admin: bool,
    pub permissions: Vec<crate::iam::Permission>,
}

#[derive(Serialize)]
pub struct WhoamiResponse {
    mode: String,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<WhoamiUserInfo>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    config_db_mismatch: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    external_providers: Vec<ExternalProviderInfo>,
}

#[derive(Serialize)]
pub struct ExternalProviderInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub provider_type: String,
    pub display_name: String,
}

#[derive(Deserialize)]
pub struct LoginAsRequest {
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Deserialize)]
pub struct ResolveIamIdentityRequest {
    access_key_id: String,
    secret_access_key: String,
}

/// Public IAM / legacy browser connect: session cookie + stored S3 creds (no full admin GUI).
#[derive(Deserialize)]
pub struct BrowserSessionConnectRequest {
    access_key_id: String,
    secret_access_key: String,
    endpoint: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    bucket: String,
}

/// Open-auth mode: `POST /api/admin/session/open-browser-connect` body.
#[derive(Deserialize)]
pub struct OpenBrowserConnectRequest {
    endpoint: String,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    bucket: String,
}

/// Whether session cookies should include the `Secure` flag (HTTPS-only).
/// Controlled by `DGP_SECURE_COOKIES`. When not set, auto-detects based on TLS:
/// TLS enabled → Secure=true; TLS disabled → Secure=false.
/// Set explicitly to `"true"` to force Secure flag regardless of TLS.
fn secure_cookies() -> bool {
    match std::env::var("DGP_SECURE_COOKIES") {
        Ok(v) if v == "true" || v == "1" => true,
        Ok(v) if v == "false" || v == "0" => false,
        _ => {
            // Auto-detect: check if TLS is enabled
            std::env::var("DGP_TLS_ENABLED")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false)
        }
    }
}

/// Format a session cookie for setting a login token.
/// Max-Age matches the session store's TTL.
pub(super) fn session_cookie(token: &str, ttl: std::time::Duration) -> String {
    let max_age = ttl.as_secs();
    let secure = if secure_cookies() { "; Secure" } else { "" };
    // Use SameSite=Lax (not Strict) to allow the cookie to be sent on top-level
    // navigations — required for OAuth callback redirects from external IdPs.
    // Lax still protects against CSRF on POST/PUT/DELETE requests.
    format!(
        "dgp_session={}; HttpOnly; SameSite=Lax; Path=/; Max-Age={}{}",
        token, max_age, secure
    )
}

/// Auto-populate S3 credentials in a session so "login IS connect".
/// Reads the backend region from config and creates an S3SessionCredentials
/// with the provided access key/secret pair.
pub(super) async fn auto_populate_s3_creds(
    state: &AdminState,
    token: &str,
    access_key_id: String,
    secret_access_key: String,
) {
    let config = state.config.read().await;
    let region = match &config.backend {
        crate::config::BackendConfig::S3 { region, .. } => region.clone(),
        _ => "us-east-1".to_string(),
    };
    state.sessions.set_s3_creds(
        token,
        S3SessionCredentials {
            endpoint: String::new(),
            region,
            bucket: String::new(),
            access_key_id,
            secret_access_key,
        },
    );
}

/// Format a session cookie that clears the login token.
pub(super) fn session_cookie_clear() -> String {
    let secure = if secure_cookies() { "; Secure" } else { "" };
    format!(
        "dgp_session=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0{}",
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

fn request_client_ip(
    headers: &HeaderMap,
    connect_info: Option<&ConnectInfo<SocketAddr>>,
) -> Option<IpAddr> {
    rate_limiter::extract_client_ip_with_peer(headers, connect_info.map(|ci| ci.0.ip()))
}

/// POST /api/admin/login — verify password, set session cookie.
pub async fn login(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req_headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    // Brute-force protection: the guard handles IP extraction, lockout
    // check, progressive delay, and lockout-transition logging.
    let guard = match crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "admin",
    )
    .await
    {
        Ok(g) => g,
        Err(_blocked) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                HeaderMap::new(),
                Json(LoginResponse { ok: false }),
            )
                .into_response();
        }
    };

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
        guard.record_failure();
        tracing::warn!("Failed login attempt from {}", guard.ip());
        audit_log("login_failed", "", "bootstrap", &req_headers);
        return (
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Json(LoginResponse { ok: false }),
        )
            .into_response();
    }

    // Successful login — reset rate limiter for this IP
    guard.record_success();
    let token = state.sessions.create_session(
        request_client_ip(&req_headers, connect_info.as_ref()),
        AuthMethod::Bootstrap,
        SessionKind::AdminGui,
    );

    // Auto-populate S3 credentials from config so "login IS connect".
    // The legacy access_key_id/secret_access_key are the proxy's own auth credentials.
    {
        let config = state.config.read().await;
        let creds = config
            .access_key_id
            .clone()
            .zip(config.secret_access_key.clone());
        let auth_on = config.auth_enabled();
        let region = match &config.backend {
            crate::config::BackendConfig::S3 { region, .. } => region.clone(),
            _ => "us-east-1".to_string(),
        };
        drop(config);
        if let Some((ak, sk)) = creds {
            auto_populate_s3_creds(&state, &token, ak, sk).await;
        } else if !auth_on {
            // Open-access deployments: no proxy SigV4 keys. Without session S3 creds,
            // a hard refresh clears the in-memory SDK and the file browser stops listing
            // (PUT/GET would fail). Mirror `open_browser_connect` anonymous pair.
            state.sessions.set_s3_creds(
                &token,
                S3SessionCredentials {
                    endpoint: String::new(),
                    region,
                    bucket: String::new(),
                    access_key_id: "anonymous".to_string(),
                    secret_access_key: "anonymous".to_string(),
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
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let client_ip = request_client_ip(&headers, connect_info.as_ref());
    let token = extract_session_token(&headers);
    let valid = token
        .as_ref()
        .map(|t| state.sessions.validate(t, client_ip))
        .unwrap_or(false);
    let admin_gui = token
        .as_ref()
        .filter(|_| valid)
        .map(|t| state.sessions.allows_admin_gui(t, client_ip))
        .unwrap_or(false);

    Json(SessionResponse { valid, admin_gui })
}

/// GET /api/whoami — returns current auth mode and (if session exists) the logged-in user.
pub async fn whoami(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Json<WhoamiResponse> {
    let iam_state = state.iam_state.load();
    let mode = match &**iam_state {
        IamState::Disabled => "open",
        IamState::Legacy(_) => "bootstrap",
        IamState::Iam(_) => "iam",
    };

    // If caller has a valid session, resolve user identity.
    let user = resolve_session_user(
        &state,
        &headers,
        request_client_ip(&headers, connect_info.as_ref()),
    )
    .await;

    // Include enabled external auth providers so the login page can show OAuth buttons.
    let external_providers = if let Some(ref ext_auth) = state.external_auth {
        if let Some(ref config_db) = state.config_db {
            let db = config_db.lock().await;
            db.load_auth_providers()
                .unwrap_or_default()
                .into_iter()
                .filter(|p| p.enabled)
                .map(|p| ExternalProviderInfo {
                    name: p.name,
                    provider_type: p.provider_type,
                    display_name: p
                        .display_name
                        .unwrap_or_else(|| "External Login".to_string()),
                })
                .collect()
        } else {
            let _ = ext_auth; // suppress unused warning
            vec![]
        }
    } else {
        vec![]
    };

    Json(WhoamiResponse {
        mode: mode.into(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        user,
        config_db_mismatch: state.config_db_mismatch,
        external_providers,
    })
}

/// POST /api/iam/identity — verify IAM S3 credentials and return the same
/// effective user permissions the SigV4 request path uses.
///
/// This does not create a session cookie (unlike [`browser_session_connect`],
/// which mints a **S3BrowserLift** cookie for hard-refresh credential restore).
pub async fn resolve_iam_identity(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req_headers: HeaderMap,
    Json(body): Json<ResolveIamIdentityRequest>,
) -> Result<Json<WhoamiResponse>, StatusCode> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "resolve_iam_identity",
    )
    .await
    .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;

    let iam_state = state.iam_state.load();
    let Some(user) = (match &**iam_state {
        IamState::Iam(index) => index.get(&body.access_key_id).cloned(),
        _ => None,
    }) else {
        guard.record_failure();
        return Err(StatusCode::FORBIDDEN);
    };

    if !iam_user_secret_valid(&user, &body.secret_access_key) {
        guard.record_failure();
        return Err(StatusCode::FORBIDDEN);
    }

    guard.record_success();

    let is_admin = crate::iam::permissions::is_admin(&user.permissions);
    Ok(Json(WhoamiResponse {
        mode: "iam".into(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        user: Some(WhoamiUserInfo {
            name: user.name,
            access_key_id: user.access_key_id,
            is_admin,
            permissions: user.permissions,
        }),
        config_db_mismatch: state.config_db_mismatch,
        external_providers: vec![],
    }))
}

/// Resolve user info from the session cookie (if present and valid).
async fn resolve_session_user(
    state: &AdminState,
    headers: &HeaderMap,
    client_ip: Option<IpAddr>,
) -> Option<WhoamiUserInfo> {
    let token = extract_session_token(headers)?;
    let auth_method = state.sessions.auth_method(&token, client_ip)?;
    match auth_method {
        crate::session::AuthMethod::OpenLift => None,
        crate::session::AuthMethod::Bootstrap => Some(WhoamiUserInfo {
            name: "admin".into(),
            access_key_id: "bootstrap".into(),
            is_admin: true,
            permissions: vec![crate::iam::Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
        }),
        crate::session::AuthMethod::IamLoginAs { access_key_id }
        | crate::session::AuthMethod::IamBrowserLift { access_key_id } => {
            let user = resolve_effective_iam_user(state, &access_key_id).await?;
            let is_admin = crate::iam::permissions::is_admin(&user.permissions);
            Some(WhoamiUserInfo {
                name: user.name,
                access_key_id: user.access_key_id,
                is_admin,
                permissions: user.permissions,
            })
        }
        crate::session::AuthMethod::External { user_id, .. } => {
            let db = state.config_db.as_ref()?.lock().await;
            let user = db.get_user_by_id(user_id).ok()?;
            // For external users, prefer external identity email > user name
            let display_name = db
                .get_external_identities_for_user(user_id)
                .ok()
                .and_then(|ids| ids.into_iter().next())
                .and_then(|ext| ext.email.or(ext.display_name))
                .unwrap_or(user.name.clone());
            drop(db);
            let effective = resolve_effective_iam_user(state, &user.access_key_id)
                .await
                .unwrap_or(user);
            let is_admin = crate::iam::permissions::is_admin(&effective.permissions);
            Some(WhoamiUserInfo {
                name: display_name,
                access_key_id: effective.access_key_id,
                is_admin,
                permissions: effective.permissions,
            })
        }
    }
}

async fn resolve_effective_iam_user(state: &AdminState, access_key_id: &str) -> Option<IamUser> {
    let iam_state = state.iam_state.load();
    if let IamState::Iam(index) = &**iam_state {
        if let Some(user) = index.get(access_key_id) {
            return Some(user.clone());
        }
    }

    let db = state.config_db.as_ref()?.lock().await;
    let user = db.get_user_by_access_key(access_key_id).ok()??;
    let groups = db.load_groups().ok()?;
    resolve_effective_iam_user_from_parts(user, groups, access_key_id)
}

fn resolve_effective_iam_user_from_parts(
    user: IamUser,
    groups: Vec<Group>,
    access_key_id: &str,
) -> Option<IamUser> {
    let index = IamIndex::from_users_and_groups(vec![user], groups);
    index.get(access_key_id).cloned()
}

/// POST /api/admin/login-as — create admin session for an IAM user with admin permissions.
/// Requires both access_key_id AND secret_access_key for authentication.
pub async fn login_as(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req_headers: HeaderMap,
    Json(body): Json<LoginAsRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "login_as",
    )
    .await
    .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;

    let iam_state = state.iam_state.load();
    let user = match &**iam_state {
        IamState::Iam(index) => index.get(&body.access_key_id),
        _ => None,
    };

    let user = match user {
        Some(u) => u,
        None => {
            guard.record_failure();
            tracing::warn!(
                "Failed login-as attempt from {} (unknown access key '{}')",
                guard.ip(),
                body.access_key_id
            );
            audit_log("login_failed", "", &body.access_key_id, &req_headers);
            return Err(StatusCode::FORBIDDEN);
        }
    };

    if !iam_user_secret_valid(user, &body.secret_access_key) {
        guard.record_failure();
        tracing::warn!(
            "Failed login-as attempt from {} (secret mismatch or disabled '{}')",
            guard.ip(),
            body.access_key_id
        );
        audit_log("login_failed", "", &body.access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN);
    }

    if !user.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }

    // Successful login — reset rate limiter
    guard.record_success();

    let token = state.sessions.create_session(
        request_client_ip(&req_headers, connect_info.as_ref()),
        AuthMethod::IamLoginAs {
            access_key_id: body.access_key_id.clone(),
        },
        SessionKind::AdminGui,
    );

    // Auto-populate S3 credentials from the IAM login so "login IS connect"
    auto_populate_s3_creds(
        &state,
        &token,
        body.access_key_id.clone(),
        body.secret_access_key.clone(),
    )
    .await;

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

/// POST /api/admin/session/browser-connect — verify S3 credentials and create a
/// **S3BrowserLift** session (cookie + stored S3 creds). Does not grant admin GUI APIs.
///
/// Used by the embedded browser for non-admin IAM users so hard refresh can restore creds.
/// IAM admins should continue to use `login-as` + `PUT session/s3-credentials`.
pub async fn browser_session_connect(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req_headers: HeaderMap,
    Json(body): Json<BrowserSessionConnectRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "browser_session_connect",
    )
    .await
    .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;

    let iam_state = state.iam_state.load();
    let access_key_id = body.access_key_id.trim();
    let secret_access_key = body.secret_access_key.as_str();

    // IAM multi-user only: legacy/bootstrap use the password connect path; open mode is separate.
    let IamState::Iam(index) = &**iam_state else {
        guard.record_failure();
        audit_log(
            "browser_session_connect_denied",
            "",
            "non_iam_mode",
            &req_headers,
        );
        return Err(StatusCode::FORBIDDEN);
    };

    let Some(user) = index.get(access_key_id) else {
        guard.record_failure();
        tracing::warn!(
            "Failed browser-session-connect from {} (unknown access key '{}')",
            guard.ip(),
            access_key_id
        );
        audit_log("login_failed", "", access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN);
    };

    if !iam_user_secret_valid(user, secret_access_key) {
        guard.record_failure();
        tracing::warn!(
            "Failed browser-session-connect from {} (secret mismatch or disabled '{}')",
            guard.ip(),
            access_key_id
        );
        audit_log("login_failed", "", access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN);
    }

    guard.record_success();
    let ak = user.access_key_id.clone();
    let ak_for_log = ak.clone();

    let region = {
        let cfg = state.config.read().await;
        let from_backend = match &cfg.backend {
            crate::config::BackendConfig::S3 { region, .. } => Some(region.clone()),
            _ => None,
        };
        drop(cfg);
        body.region
            .filter(|r| !r.is_empty())
            .or(from_backend)
            .unwrap_or_else(|| "us-east-1".to_string())
    };

    let token = state.sessions.create_session(
        request_client_ip(&req_headers, connect_info.as_ref()),
        AuthMethod::IamBrowserLift {
            access_key_id: ak.clone(),
        },
        SessionKind::S3BrowserLift,
    );

    state.sessions.set_s3_creds(
        &token,
        S3SessionCredentials {
            endpoint: body.endpoint,
            region,
            bucket: body.bucket,
            access_key_id: ak,
            secret_access_key: body.secret_access_key,
        },
    );

    tracing::info!(
        "S3 browser-lift session created for access key '{}' from {}",
        ak_for_log,
        guard.ip()
    );

    audit_log(
        "browser_session_connect",
        &user.name,
        &ak_for_log,
        &req_headers,
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

/// POST /api/admin/session/open-browser-connect — `authentication: none` only.
/// Mints **S3BrowserLift** + anonymous S3 creds so open-mode UI survives hard refresh.
pub async fn open_browser_connect(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    req_headers: HeaderMap,
    Json(body): Json<OpenBrowserConnectRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "open_browser_connect",
    )
    .await
    .map_err(|_| StatusCode::TOO_MANY_REQUESTS)?;

    let iam_state = state.iam_state.load();
    if !matches!(&**iam_state, IamState::Disabled) {
        guard.record_failure();
        audit_log(
            "open_browser_connect_denied",
            "",
            "auth_required",
            &req_headers,
        );
        return Err(StatusCode::FORBIDDEN);
    }

    guard.record_success();

    let region = {
        let cfg = state.config.read().await;
        let from_backend = match &cfg.backend {
            crate::config::BackendConfig::S3 { region, .. } => Some(region.clone()),
            _ => None,
        };
        drop(cfg);
        body.region
            .filter(|r| !r.is_empty())
            .or(from_backend)
            .unwrap_or_else(|| "us-east-1".to_string())
    };

    let token = state.sessions.create_session(
        request_client_ip(&req_headers, connect_info.as_ref()),
        AuthMethod::OpenLift,
        SessionKind::S3BrowserLift,
    );

    state.sessions.set_s3_creds(
        &token,
        S3SessionCredentials {
            endpoint: body.endpoint,
            region,
            bucket: body.bucket,
            access_key_id: "anonymous".to_string(),
            secret_access_key: "anonymous".to_string(),
        },
    );

    audit_log("open_browser_connect", "open", "anonymous", &req_headers);

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
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let client_ip = rate_limiter::extract_client_ip_with_peer(&headers, peer_ip);
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

/// Middleware: valid **AdminGui** session only (rejects S3BrowserLift cookies).
pub async fn require_admin_gui_session(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let client_ip = rate_limiter::extract_client_ip_with_peer(&headers, peer_ip);
    let Some(token) = extract_session_token(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    };
    if !state.sessions.allows_admin_gui(&token, client_ip) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin_session_required"})),
        )
            .into_response();
    }

    request.extensions_mut().insert(AdminGuiGate);
    next.run(request).await.into_response()
}

/// Middleware: reject IAM mutation requests when `access.iam_mode` is
/// `Declarative`. The YAML document is the source of truth in that
/// mode, and a runtime GUI/API mutation would silently diverge from it
/// (until the next `apply` overwrites the change).
///
/// Applied to `POST/PUT/DELETE` routes under:
///   - `/api/admin/users/*`
///   - `/api/admin/groups/*`
///   - `/api/admin/ext-auth/providers/*`
///   - `/api/admin/ext-auth/mappings/*`
///   - `/api/admin/ext-auth/sync-memberships`
///   - `/api/admin/migrate`
///
/// Read endpoints (`GET`) are allowed — the GUI should still be able
/// to display the DB state for diagnostics. Write endpoints return
/// 403 with an explanatory body pointing to the declarative workflow.
pub async fn require_not_declarative(
    State(state): State<Arc<AdminState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    // Read routes are allowed even in declarative mode — diagnostics
    // still need to show DB state.
    let method = request.method();
    let is_mutation = matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    if !is_mutation {
        return next.run(request).await.into_response();
    }

    // Check the current runtime mode. We do NOT cache this — config is
    // hot-reloadable and a toggle between GUI and Declarative should
    // take effect on the very next request.
    let cfg = state.config.read().await;
    let is_declarative = matches!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
    drop(cfg);

    if !is_declarative {
        return next.run(request).await.into_response();
    }

    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "iam_declarative",
            "message": "IAM is managed via the YAML document (access.iam_mode: declarative). \
                        Edit your config file and POST the full document to /api/admin/config/apply \
                        instead of mutating users/groups/providers through this endpoint.",
        })),
    )
        .into_response()
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
    use crate::iam::{AuthConfig, Permission, SharedIamState};
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

    #[test]
    fn resolve_effective_iam_user_from_parts_merges_group_permissions() {
        let user = IamUser {
            id: 1,
            name: "alice".into(),
            access_key_id: "AKALICE".into(),
            secret_access_key: "secret".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec!["artifacts/alice/*".into()],
                conditions: None,
            }],
            group_ids: vec![10],
            auth_source: "local".into(),
            iam_policies: vec![],
        };
        let groups = vec![Group {
            id: 10,
            name: "writers".into(),
            description: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["write".into()],
                resources: vec!["artifacts/alice/*".into()],
                conditions: None,
            }],
            member_ids: vec![1],
            created_at: String::new(),
        }];

        let effective =
            resolve_effective_iam_user_from_parts(user, groups, "AKALICE").expect("effective user");

        assert_eq!(effective.permissions.len(), 2);
        assert!(effective
            .permissions
            .iter()
            .any(|p| p.actions == vec!["read"]));
        assert!(effective
            .permissions
            .iter()
            .any(|p| p.actions == vec!["write"]));
        assert_eq!(effective.iam_policies.len(), 2);
    }
}
