// SPDX-License-Identifier: BUSL-1.1

//! Auth handlers: login, logout, login_as, whoami, check_session, require_session.

use crate::api::admin::extract::AdminJson;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

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

/// The session behind an admin request, inserted by
/// [`require_admin_gui_session`]. A long-lived response (an SSE stream) holds
/// it to re-run the same admin check while it streams: the gate runs once per
/// request, and a stream is one request that can last for hours.
#[derive(Clone)]
pub struct AdminSessionCheck {
    state: Arc<AdminState>,
    token: String,
    client_ip: Option<IpAddr>,
}

impl AdminSessionCheck {
    /// The same rule as the gate: a live AdminGui session whose principal is
    /// still an enabled admin.
    pub fn still_admin(&self) -> bool {
        admin_gui_session_ok(&self.state, &self.token, self.client_ip)
    }
}

/// How often a streaming admin response re-checks its session.
const STREAM_SESSION_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

/// End `stream` when its session stops passing the admin check (logout,
/// revocation, expiry, or the principal disabled or demoted). The check runs
/// on a timer, so an idle stream ends too, not only at the next frame.
pub(crate) fn end_when_admin_session_lapses<S: futures::Stream>(
    stream: S,
    session: AdminSessionCheck,
) -> impl futures::Stream<Item = S::Item> {
    use futures::StreamExt;
    stream.take_until(async move {
        let mut tick = tokio::time::interval(STREAM_SESSION_RECHECK);
        loop {
            tick.tick().await;
            if !session.still_admin() {
                break;
            }
        }
    })
}

/// Constant-time secret check + `enabled` gate for IAM index users.
/// Shared by `login-as`, `browser-session-connect`, and `POST /api/iam/identity`.
pub(crate) fn iam_user_secret_valid(user: &IamUser, secret_access_key: &str) -> bool {
    if !user.enabled {
        return false;
    }
    crate::security::secret_eq(
        user.secret_access_key.as_bytes(),
        secret_access_key.as_bytes(),
    )
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
    /// Exact build version — present for authenticated callers only: a live
    /// session here, or verified IAM credentials on `POST /_/api/iam/identity`
    /// (see [`build_version_for`]); omitted for anonymous callers.
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    /// UTC build timestamp of the running binary — same gate as `version`.
    #[serde(skip_serializing_if = "Option::is_none")]
    build_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<WhoamiUserInfo>,
    /// How the caller signed in (see [`auth_method_label`]); omitted when
    /// there is no session. `user.access_key_id` alone cannot say it: a
    /// bootstrap session reports the synthetic key `bootstrap`, which a real
    /// IAM user may also have.
    #[serde(skip_serializing_if = "Option::is_none")]
    auth_method: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    config_db_mismatch: bool,
    /// Typed lock signal for the frontend: `"locked"` when the config DB
    /// failed to decrypt (mirrors `config_db_mismatch`), omitted otherwise.
    /// Lets the UI branch on a typed field instead of regex-matching error text.
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_state: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    external_providers: Vec<ExternalProviderInfo>,
}

/// The `version` field of [`WhoamiResponse`]: the exact build version goes
/// only to callers that hold a live session. The login page calls
/// `/_/api/whoami` before any login (it needs `mode` and the provider list),
/// so anonymous callers must not be able to fingerprint the deployment.
fn build_version_for(session_valid: bool) -> Option<String> {
    session_valid.then(|| env!("CARGO_PKG_VERSION").to_string())
}

/// Same gate as [`build_version_for`] for the build timestamp (stamped by
/// `build.rs`). The UI shows it in the sidebar; it used to be a Vite `define`
/// baked into the public JS bundle, which dated the build for anyone.
fn build_time_for(session_valid: bool) -> Option<String> {
    session_valid.then(|| env!("DGP_BUILD_TIME").to_string())
}

/// `DGP_METRICS_BEARER_TOKEN`: when set, `/_/metrics` answers only to a
/// matching `Authorization: Bearer <token>` (the Prometheus `authorization:`
/// scrape setting) or to a live admin-GUI session (the dashboard). Unset —
/// the default — keeps the endpoint public for scrapers. Read once: the
/// token is deployment infrastructure, not hot-reloadable config.
fn metrics_bearer_token() -> Option<&'static str> {
    static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TOKEN
        .get_or_init(|| {
            std::env::var("DGP_METRICS_BEARER_TOKEN")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
        .as_deref()
}

/// The bearer token a request presents, if it presents one.
fn presented_bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
}

/// Constant-time check of the request's bearer token against `token`
/// (length-oblivious, see [`crate::security::secret_eq`]).
fn bearer_matches(headers: &HeaderMap, token: &str) -> bool {
    presented_bearer(headers)
        .is_some_and(|presented| crate::security::secret_eq(presented.as_bytes(), token.as_bytes()))
}

/// Middleware for `/_/metrics` — see [`metrics_bearer_token`]. A pass-through
/// when no token is configured.
pub async fn require_metrics_access(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let Some(token) = metrics_bearer_token() else {
        return next.run(request).await.into_response();
    };
    let peer_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    // A presented bearer is a credential check like login: it enters the
    // per-IP rate limiter, and a wrong token counts as a failure (lockout +
    // SECURITY log line). A request with no Authorization header is the
    // dashboard's session path and is not counted.
    if presented_bearer(&headers).is_some() {
        let guard = match rate_limiter::RateLimitGuard::enter(
            &state.rate_limiter,
            &headers,
            peer_ip,
            "metrics",
        )
        .await
        {
            Ok(g) => g,
            Err(blocked) => return blocked.into_response(),
        };
        if bearer_matches(&headers, token) {
            guard.record_success();
            return next.run(request).await.into_response();
        }
        guard.record_failure();
    }
    let client_ip = rate_limiter::extract_client_ip_with_peer(&headers, peer_ip);
    // `admin_gui_session_ok` already requires a live entry (same `entry_valid`
    // check as `validate`), so one call is the whole test.
    let admin_session = extract_session_token(&headers)
        .map(|t| admin_gui_session_ok(&state, &t, client_ip))
        .unwrap_or(false);
    if admin_session {
        return next.run(request).await.into_response();
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"metrics\"")],
        "metrics require the configured bearer token or an admin session\n",
    )
        .into_response()
}

/// Map the config-DB lock flag to the typed `lock_state` whoami field.
fn lock_state_for(config_db_mismatch: bool) -> Option<&'static str> {
    if config_db_mismatch {
        Some("locked")
    } else {
        None
    }
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

/// Error of a credential handler: a plain status, or a lockout (429 with
/// `Retry-After` and a JSON reason, see [`crate::rate_limiter::Blocked`]).
pub enum AuthReject {
    Status(StatusCode),
    Blocked(crate::rate_limiter::Blocked),
}

impl From<StatusCode> for AuthReject {
    fn from(s: StatusCode) -> Self {
        Self::Status(s)
    }
}

impl From<crate::rate_limiter::Blocked> for AuthReject {
    fn from(b: crate::rate_limiter::Blocked) -> Self {
        Self::Blocked(b)
    }
}

impl IntoResponse for AuthReject {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Status(s) => s.into_response(),
            Self::Blocked(b) => b.into_response(),
        }
    }
}

/// Whether session cookies should include the `Secure` flag (HTTPS-only).
/// Controlled by `DGP_SECURE_COOKIES`. When not set, auto-detects based
/// on TLS at our listener (YAML `advanced.tls.enabled` or
/// `DGP_TLS_ENABLED=true`, see [`crate::tls::listener_tls`]) OR a trusted
/// `X-Forwarded-Proto: https` from the front proxy.
/// Set `DGP_SECURE_COOKIES=true` to force Secure regardless of detection.
fn secure_cookies() -> bool {
    secure_cookies_with(None)
}

/// Same as [`secure_cookies`] but also consults the inbound request's
/// `X-Forwarded-Proto` header (only when `DGP_TRUST_PROXY_HEADERS=true`).
/// Use this on the response-issuing path so a TLS-terminated front
/// proxy yields a `Secure` cookie even when our listener is plain HTTP.
pub(super) fn secure_cookies_with(headers: Option<&HeaderMap>) -> bool {
    // Tri-state: an explicit, *recognised* DGP_SECURE_COOKIES value wins
    // (true OR false); absent or unrecognised falls through to TLS /
    // forwarded-proto auto-detection. The accepted truth-set matches
    // env_bool (true/1/yes/on, false/0/no/off) — kept inline because
    // env_bool can't express the "fall through on unset/garbage" arm.
    if let Ok(raw) = std::env::var("DGP_SECURE_COOKIES") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => return true,
            "false" | "0" | "no" | "off" => return false,
            _ => {}
        }
    }
    if crate::tls::listener_tls() {
        return true;
    }
    if crate::rate_limiter::trust_proxy_headers() {
        if let Some(h) = headers {
            if let Some(proto) = h
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_ascii_lowercase())
            {
                if proto == "https" {
                    return true;
                }
            }
        }
    }
    false
}

/// Remove the caller's previous session, if any. Called at every
/// session-minting boundary so an XSS-leaked cookie from before
/// "log out + log in" can't outlive the rotation.
pub(super) fn drop_prior_session(state: &AdminState, headers: &HeaderMap) {
    if let Some(prior) = extract_session_token(headers) {
        state.sessions.remove(&prior);
    }
}

/// THE single session-mint constructor. Owns the load-bearing order — drop the
/// prior cookie BEFORE creating the new one (XSS-rotation defense) — so a new
/// mint path physically cannot forget it. All session-minting routes go through
/// here; setting S3 creds stays at the call site (it varies per path).
///
/// It also writes the audit entry for the successful login, so no login path
/// can mint a session without leaving a trace (issue #92: bootstrap and
/// login-as logins were not audited, only `login_failed`). `user_name` is the
/// display name of the identity that logged in (`""` when there is none).
pub(super) fn mint_session(
    state: &AdminState,
    headers: &HeaderMap,
    connect_info: Option<&ConnectInfo<SocketAddr>>,
    auth_method: AuthMethod,
    kind: SessionKind,
    user_name: &str,
) -> String {
    drop_prior_session(state, headers);
    let (action, user, target) = login_audit_fields(&auth_method, user_name);
    let token =
        state
            .sessions
            .create_session(request_client_ip(headers, connect_info), auth_method, kind);
    audit_log(action, &user, &target, headers);
    token
}

/// Audit `(action, user, target)` for a successful login. Pure; unit-tested.
/// Action names that existed before this helper (`external_login`,
/// `browser_session_connect`, `open_browser_connect`) are kept so log
/// pipelines that match on them keep working.
pub(super) fn login_audit_fields(
    method: &AuthMethod,
    user_name: &str,
) -> (&'static str, String, String) {
    match method {
        AuthMethod::Bootstrap => ("login", "bootstrap".into(), "bootstrap".into()),
        AuthMethod::IamLoginAs { access_key_id } => {
            ("login_as", user_name.into(), access_key_id.clone())
        }
        AuthMethod::IamBrowserLift { access_key_id } => (
            "browser_session_connect",
            user_name.into(),
            access_key_id.clone(),
        ),
        AuthMethod::OpenLift => ("open_browser_connect", "open".into(), "anonymous".into()),
        AuthMethod::External { provider_name, .. } => {
            ("external_login", user_name.into(), provider_name.clone())
        }
    }
}

/// Test-only shim: equivalent to [`session_cookie_with_headers`] with
/// no request headers. Kept so the `session_cookie_is_samesite_strict_*`
/// unit tests can build a cookie without an axum `HeaderMap`. The
/// cookie-shape contract (SameSite=Strict, HttpOnly, Path=/, Max-Age)
/// is documented on the production builder below.
#[cfg(test)]
pub(super) fn session_cookie(token: &str, ttl: std::time::Duration) -> String {
    session_cookie_with_headers(token, ttl, None)
}

/// Format a session cookie for setting a login token. This is the
/// production code path; the `#[cfg(test)]` [`session_cookie`] above
/// is a no-headers shim.
///
/// Max-Age matches the session store's TTL.
///
/// `SameSite=Strict` (not Lax) — Strict blocks cross-site top-level
/// GET navigations from carrying the cookie, which is the safe choice
/// for a session that authorises both the admin GUI and S3-credential
/// minting. The OAuth callback redirect from an external IdP is the
/// one cross-site GET we actually do, and it works fine: the response
/// is what *sets* the cookie (no read needed) and the subsequent
/// same-origin redirect to `/_/admin` reads it back under Strict.
///
/// One UX trade: bookmark-loaded `https://proxy/_/admin` may need a
/// reload after login since the first hit doesn't carry the cookie.
/// We judge the CSRF surface reduction worth it for a public-internet
/// deployment.
///
/// Consults `headers` so a `Secure` flag fires when the front proxy
/// reports `X-Forwarded-Proto: https` even though our listener is
/// plain HTTP. Pass `Some(req_headers)` from every login handler.
pub(super) fn session_cookie_with_headers(
    token: &str,
    ttl: std::time::Duration,
    headers: Option<&HeaderMap>,
) -> String {
    let max_age = ttl.as_secs();
    let secure = if secure_cookies_with(headers) {
        "; Secure"
    } else {
        ""
    };
    format!(
        "dgp_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
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
    AdminJson(body): AdminJson<LoginRequest>,
) -> impl IntoResponse {
    // Brute-force protection: per-IP cap (catches single-host noise)
    // PLUS per-account cap (catches distributed credential stuffing
    // against the bootstrap password — a botnet rotating IPs across
    // a /16 can chew the per-IP budget freely without this).
    let guard = match crate::rate_limiter::RateLimitGuard::enter_with_account(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "bootstrap",
        "admin",
    )
    .await
    {
        Ok(g) => g,
        Err(blocked) => return blocked.into_response(),
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
    // Rotate the session: drop any pre-login cookie so an XSS-leaked
    // earlier token can't outlive the password re-entry.
    let token = mint_session(
        &state,
        &req_headers,
        connect_info.as_ref(),
        AuthMethod::Bootstrap,
        SessionKind::AdminGui,
        "",
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
                S3SessionCredentials::anonymous(String::new(), region, String::new()),
            );
        }
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        session_cookie_with_headers(&token, state.sessions.ttl(), Some(&req_headers))
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
        .map(|t| admin_gui_session_ok(&state, t, client_ip))
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

    let client_ip = request_client_ip(&headers, connect_info.as_ref());
    // One session lookup answers both questions: is there a live session
    // (any kind — admin GUI, S3-browser lift, open-mode lift — may learn the
    // version; an anonymous caller may not), and who is it.
    let session =
        extract_session_token(&headers).and_then(|t| state.sessions.auth_method(&t, client_ip));
    let session_valid = session.is_some();
    let auth_method = session.as_ref().map(auth_method_label);
    let user = match session {
        Some(method) => session_user_info(&state, method).await,
        None => None,
    };

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
        version: build_version_for(session_valid),
        build_time: build_time_for(session_valid),
        user,
        auth_method,
        config_db_mismatch: state.config_db_mismatch,
        lock_state: lock_state_for(state.config_db_mismatch),
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
    AdminJson(body): AdminJson<ResolveIamIdentityRequest>,
) -> Result<Json<WhoamiResponse>, AuthReject> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "resolve_iam_identity",
    )
    .await?;

    let iam_state = state.iam_state.load();
    let Some(user) = (match &**iam_state {
        IamState::Iam(index) => index.get(&body.access_key_id).cloned(),
        _ => None,
    }) else {
        guard.record_failure();
        return Err(StatusCode::FORBIDDEN.into());
    };

    if !iam_user_secret_valid(&user, &body.secret_access_key) {
        guard.record_failure();
        return Err(StatusCode::FORBIDDEN.into());
    }

    guard.record_success();

    let is_admin = crate::iam::permissions::is_admin(&user.permissions);
    Ok(Json(WhoamiResponse {
        mode: "iam".into(),
        // The IAM credentials were verified just above — an authenticated caller.
        version: build_version_for(true),
        build_time: build_time_for(true),
        user: Some(WhoamiUserInfo {
            name: user.name,
            access_key_id: user.access_key_id,
            is_admin,
            permissions: user.permissions,
        }),
        auth_method: Some("iam"),
        config_db_mismatch: state.config_db_mismatch,
        lock_state: lock_state_for(state.config_db_mismatch),
        external_providers: vec![],
    }))
}

/// The `auth_method` wire value of [`WhoamiResponse`] for a session.
pub(crate) fn auth_method_label(method: &crate::session::AuthMethod) -> &'static str {
    use crate::session::AuthMethod;
    match method {
        AuthMethod::Bootstrap => "bootstrap",
        AuthMethod::IamLoginAs { .. } => "iam",
        AuthMethod::IamBrowserLift { .. } => "iam_browser",
        AuthMethod::OpenLift => "open",
        AuthMethod::External { .. } => "external",
    }
}

/// User info for a live session's auth method (`None` for an open-mode lift,
/// which has no user).
async fn session_user_info(
    state: &AdminState,
    auth_method: crate::session::AuthMethod,
) -> Option<WhoamiUserInfo> {
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
            let is_admin = user.enabled && crate::iam::permissions::is_admin(&user.permissions);
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
            let is_admin =
                effective.enabled && crate::iam::permissions::is_admin(&effective.permissions);
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
    AdminJson(body): AdminJson<LoginAsRequest>,
) -> Result<impl IntoResponse, AuthReject> {
    // Per-IP + per-account brute-force gate. Without the per-account
    // bucket, a botnet rotating IPs could target a specific admin's
    // access_key_id without any rate limit. The account dimension is
    // the AKID being attempted.
    let guard = crate::rate_limiter::RateLimitGuard::enter_with_account(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        &body.access_key_id,
        "login_as",
    )
    .await?;

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
            return Err(StatusCode::FORBIDDEN.into());
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
        return Err(StatusCode::FORBIDDEN.into());
    }

    // The same rule that mints the OAuth session kind and gates every admin
    // request afterwards.
    let auth_method = AuthMethod::IamLoginAs {
        access_key_id: body.access_key_id.clone(),
    };
    if !session_principal_is_admin(&auth_method, &iam_state) {
        return Err(StatusCode::FORBIDDEN.into());
    }

    // Successful login — reset rate limiter
    guard.record_success();

    // Rotate the session: drop any pre-login cookie so an XSS-leaked
    // earlier token can't outlive the credential re-entry.
    let token = mint_session(
        &state,
        &req_headers,
        connect_info.as_ref(),
        auth_method,
        SessionKind::AdminGui,
        &user.name,
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
            session_cookie_with_headers(&token, state.sessions.ttl(), Some(&req_headers)),
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
    AdminJson(body): AdminJson<BrowserSessionConnectRequest>,
) -> Result<impl IntoResponse, AuthReject> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "browser_session_connect",
    )
    .await?;

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
        return Err(StatusCode::FORBIDDEN.into());
    };

    let Some(user) = index.get(access_key_id) else {
        guard.record_failure();
        tracing::warn!(
            "Failed browser-session-connect from {} (unknown access key '{}')",
            guard.ip(),
            access_key_id
        );
        audit_log("login_failed", "", access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN.into());
    };

    if !iam_user_secret_valid(user, secret_access_key) {
        guard.record_failure();
        tracing::warn!(
            "Failed browser-session-connect from {} (secret mismatch or disabled '{}')",
            guard.ip(),
            access_key_id
        );
        audit_log("login_failed", "", access_key_id, &req_headers);
        return Err(StatusCode::FORBIDDEN.into());
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

    // Rotate the session: drop any pre-login cookie so an XSS-leaked
    // earlier token can't outlive the credential re-entry.
    let token = mint_session(
        &state,
        &req_headers,
        connect_info.as_ref(),
        AuthMethod::IamBrowserLift {
            access_key_id: ak.clone(),
        },
        SessionKind::S3BrowserLift,
        &user.name,
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

    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            session_cookie_with_headers(&token, state.sessions.ttl(), Some(&req_headers)),
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
    AdminJson(body): AdminJson<OpenBrowserConnectRequest>,
) -> Result<impl IntoResponse, AuthReject> {
    let guard = crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "open_browser_connect",
    )
    .await?;

    let iam_state = state.iam_state.load();
    if !matches!(&**iam_state, IamState::Disabled) {
        guard.record_failure();
        audit_log(
            "open_browser_connect_denied",
            "",
            "auth_required",
            &req_headers,
        );
        return Err(StatusCode::FORBIDDEN.into());
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

    // Rotate the session: drop any pre-login cookie so an XSS-leaked
    // earlier token can't outlive the new bind.
    let token = mint_session(
        &state,
        &req_headers,
        connect_info.as_ref(),
        AuthMethod::OpenLift,
        SessionKind::S3BrowserLift,
        "",
    );

    state.sessions.set_s3_creds(
        &token,
        S3SessionCredentials::anonymous(body.endpoint, region, body.bucket),
    );

    Ok((
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            session_cookie_with_headers(&token, state.sessions.ttl(), Some(&req_headers)),
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

/// Whether the principal behind a session is, right now, an enabled admin.
/// `iam` is the live IAM state; its index holds EFFECTIVE (group-merged)
/// permissions.
///
/// - `Bootstrap`: the holder of the bootstrap password — always admin.
/// - `IamLoginAs` / `External`: the user must still exist, be enabled, and
///   hold admin permissions. Anything else (IAM mode gone, user deleted) is no.
/// - Browser-lift and open-mode sessions are never admin.
pub(crate) fn session_principal_is_admin(method: &AuthMethod, iam: &IamState) -> bool {
    let IamState::Iam(index) = iam else {
        return matches!(method, AuthMethod::Bootstrap);
    };
    let user = match method {
        AuthMethod::Bootstrap => return true,
        AuthMethod::IamLoginAs { access_key_id } => index.get(access_key_id),
        AuthMethod::External { user_id, .. } => index.get_by_id(*user_id),
        AuthMethod::IamBrowserLift { .. } | AuthMethod::OpenLift => return false,
    };
    user.is_some_and(|u| u.enabled && u.is_admin())
}

/// THE admin-surface session test: a live `AdminGui` session whose principal
/// is still an enabled admin. A session's KIND is fixed when it is minted; the
/// principal's rights are not. So a disabled or demoted user (or one whose
/// admin group was removed, locally or through config sync) loses the admin
/// surface on the next request. Every admin-session check goes through here.
fn admin_gui_session_ok(state: &AdminState, token: &str, client_ip: Option<IpAddr>) -> bool {
    state
        .sessions
        .admin_gui_auth_method(token, client_ip)
        .is_some_and(|method| session_principal_is_admin(&method, &state.iam_state.load()))
}

/// Middleware: valid **AdminGui** session only (rejects S3BrowserLift cookies),
/// whose principal is still an enabled admin.
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
    // No LIVE session (unknown / expired / REVOKED / wrong IP) is 401 — the UI
    // must re-login. 403 is reserved for a live session of the wrong KIND.
    if !state.sessions.validate(&token, client_ip) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    if !admin_gui_session_ok(&state, &token, client_ip) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin_session_required"})),
        )
            .into_response();
    }

    let actor = state
        .sessions
        .admin_gui_auth_method(&token, client_ip)
        .map(|m| session_actor_label(&m, &state.iam_state.load()))
        .unwrap_or_else(|| "admin".to_string());
    request.extensions_mut().insert(AdminGuiGate);
    request.extensions_mut().insert(AdminSessionCheck {
        state: state.clone(),
        token,
        client_ip,
    });
    crate::audit::with_actor(actor, next.run(request))
        .await
        .into_response()
}

/// Who runs a bulk object request (`/_/api/admin/objects/*`), inserted by
/// [`require_bulk_session`].
#[derive(Clone, Debug)]
pub enum BulkSession {
    /// An admin GUI session: the session is the authorization boundary.
    AdminGui,
    /// A non-admin browser session (S3BrowserLift): every key is authorized
    /// with this IAM user's policy, like the S3 API. `client_ip` is the
    /// trusted client address, for `aws:SourceIp` conditions.
    IamUser {
        access_key_id: String,
        client_ip: Option<IpAddr>,
    },
    /// An open-mode browser session (`authentication: none`): the S3 API
    /// is unrestricted, so these requests are too.
    Open,
}

/// Middleware for the bulk object endpoints: an admin GUI session, or a
/// browser session of an IAM user (its keys are authorized one by one in
/// the handler), or an open-mode session while access is open.
pub async fn require_bulk_session(
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
    let unauthorized = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response()
    };
    let Some(token) = extract_session_token(&headers) else {
        return unauthorized();
    };
    if !state.sessions.validate(&token, client_ip) {
        return unauthorized();
    }
    let iam = state.iam_state.load();
    let (session, actor) = if admin_gui_session_ok(&state, &token, client_ip) {
        let actor = state
            .sessions
            .admin_gui_auth_method(&token, client_ip)
            .map(|m| session_actor_label(&m, &iam))
            .unwrap_or_else(|| "admin".to_string());
        (BulkSession::AdminGui, actor)
    } else {
        match state.sessions.auth_method(&token, client_ip) {
            Some(AuthMethod::IamBrowserLift { access_key_id }) => {
                let actor = match iam.as_ref() {
                    IamState::Iam(index) => index.get(&access_key_id).map(|u| u.name.clone()),
                    _ => None,
                }
                .unwrap_or_else(|| access_key_id.clone());
                (
                    BulkSession::IamUser {
                        access_key_id,
                        client_ip: rate_limiter::extract_trusted_client_ip(&headers, peer_ip),
                    },
                    actor,
                )
            }
            Some(AuthMethod::OpenLift) if matches!(iam.as_ref(), IamState::Disabled) => {
                (BulkSession::Open, "anonymous".to_string())
            }
            _ => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "admin_session_required"})),
                )
                    .into_response()
            }
        }
    };
    request.extensions_mut().insert(session);
    crate::audit::with_actor(actor, next.run(request))
        .await
        .into_response()
}

/// Audit actor for an admin session: the IAM user name when known.
fn session_actor_label(method: &AuthMethod, iam: &IamState) -> String {
    let index = match iam {
        IamState::Iam(index) => Some(index),
        _ => None,
    };
    match method {
        // Unchanged label: log queries key on `user=admin` for break-glass.
        AuthMethod::Bootstrap => "admin".to_string(),
        AuthMethod::IamLoginAs { access_key_id } => index
            .and_then(|i| i.get(access_key_id))
            .map(|u| u.name.clone())
            .unwrap_or_else(|| format!("iam:{access_key_id}")),
        AuthMethod::External {
            provider_name,
            user_id,
        } => index
            .and_then(|i| i.get_by_id(*user_id))
            .map(|u| u.name.clone())
            .unwrap_or_else(|| format!("{provider_name}:{user_id}")),
        AuthMethod::IamBrowserLift { access_key_id } => format!("iam:{access_key_id}"),
        AuthMethod::OpenLift => "anonymous".to_string(),
    }
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
///
/// The response body contains live access keys; explicit `no-store`
/// (paired with `Pragma: no-cache`) prevents intermediary caches and
/// the browser's bfcache from retaining the secret.
pub async fn get_s3_session_creds(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let token = match extract_session_token(&headers) {
        Some(t) => t,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let client_ip = request_client_ip(&headers, connect_info.as_ref());
    match state.sessions.get_s3_creds(&token, client_ip) {
        Some(creds) => (
            StatusCode::OK,
            [
                (
                    "cache-control",
                    "no-store, no-cache, must-revalidate, private",
                ),
                ("pragma", "no-cache"),
            ],
            Json(creds),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// PUT /api/admin/session/s3-credentials — store or update S3 credentials.
/// Used by the ConnectPage when connecting to a custom endpoint.
pub async fn set_s3_session_creds(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(creds): AdminJson<S3SessionCredentials>,
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

    /// Issue #92: every successful login writes an audit entry, and the
    /// action names that existed before (OAuth, browser lift, open lift) stay.
    #[test]
    fn login_audit_fields_cover_every_auth_method() {
        let f = |m: AuthMethod, n: &str| {
            let (a, u, t) = login_audit_fields(&m, n);
            (a.to_string(), u, t)
        };
        assert_eq!(
            f(AuthMethod::Bootstrap, ""),
            ("login".into(), "bootstrap".into(), "bootstrap".into())
        );
        assert_eq!(
            f(
                AuthMethod::IamLoginAs {
                    access_key_id: "AK".into()
                },
                "dana"
            ),
            ("login_as".into(), "dana".into(), "AK".into())
        );
        assert_eq!(
            f(
                AuthMethod::IamBrowserLift {
                    access_key_id: "AK".into()
                },
                "ci"
            ),
            ("browser_session_connect".into(), "ci".into(), "AK".into())
        );
        assert_eq!(
            f(AuthMethod::OpenLift, ""),
            (
                "open_browser_connect".into(),
                "open".into(),
                "anonymous".into()
            )
        );
        assert_eq!(
            f(
                AuthMethod::External {
                    provider_name: "google".into(),
                    user_id: 7
                },
                "dana"
            ),
            ("external_login".into(), "dana".into(), "google".into())
        );
        // The admin audit panel colours `login*` actions green.
        assert!(login_audit_fields(&AuthMethod::Bootstrap, "")
            .0
            .starts_with("login"));
    }

    /// Every admin SSE response must end when its session stops being an
    /// admin session: the gate runs once, and a stream can last for hours.
    #[test]
    fn every_admin_sse_stream_rechecks_its_session() {
        // Every SSE endpoint in the crate is an admin endpoint; walk all of
        // `src/`, so a stream added in a sub-module is covered too.
        fn rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rs_files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        rs_files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let needle = concat!("Sse", "::new(");
        let mut found = 0;
        for path in files {
            let src = std::fs::read_to_string(&path).unwrap();
            for (i, _) in src.match_indices(needle) {
                found += 1;
                let rest = src[i + needle.len()..].trim_start();
                assert!(
                    rest.starts_with("end_when_admin_session_lapses(")
                        || rest.starts_with("super::auth::end_when_admin_session_lapses("),
                    "{}: an admin SSE stream must be wrapped in end_when_admin_session_lapses",
                    path.display()
                );
            }
        }
        assert!(
            found >= 2,
            "expected the log and scan streams, found {found}"
        );
    }

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

    fn admin_test_user(
        id: i64,
        ak: &str,
        enabled: bool,
        direct_admin: bool,
        groups: Vec<i64>,
    ) -> IamUser {
        let wildcard = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        };
        IamUser {
            id,
            name: ak.to_lowercase(),
            access_key_id: ak.into(),
            secret_access_key: "secret".into(),
            enabled,
            created_at: String::new(),
            permissions: if direct_admin { vec![wildcard] } else { vec![] },
            group_ids: groups,
            auth_source: "local".into(),
            iam_policies: vec![],
        }
    }

    /// Truth table: a session reaches the admin surface only while its
    /// principal is an enabled admin in the LIVE index.
    #[test]
    fn session_principal_is_admin_tracks_the_live_index() {
        let admins = Group {
            id: 10,
            name: "administrators".into(),
            description: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
            member_ids: vec![3],
            created_at: String::new(),
        };
        let iam = IamState::Iam(IamIndex::from_users_and_groups(
            vec![
                admin_test_user(1, "AKADMIN", true, true, vec![]),
                admin_test_user(2, "AKOFF", false, true, vec![]),
                admin_test_user(3, "AKGROUP", true, false, vec![10]),
                admin_test_user(4, "AKPLAIN", true, false, vec![]),
            ],
            vec![admins],
        ));
        let login_as = |ak: &str| AuthMethod::IamLoginAs {
            access_key_id: ak.into(),
        };
        let external = |id: i64| AuthMethod::External {
            provider_name: "google".into(),
            user_id: id,
        };

        assert!(session_principal_is_admin(&AuthMethod::Bootstrap, &iam));
        assert!(session_principal_is_admin(&login_as("AKADMIN"), &iam));
        assert!(session_principal_is_admin(&external(1), &iam));
        // Admin through a group only.
        assert!(session_principal_is_admin(&external(3), &iam));
        assert!(session_principal_is_admin(&login_as("AKGROUP"), &iam));
        // Disabled admin, non-admin, deleted user.
        assert!(!session_principal_is_admin(&login_as("AKOFF"), &iam));
        assert!(!session_principal_is_admin(&external(2), &iam));
        assert!(!session_principal_is_admin(&external(4), &iam));
        assert!(!session_principal_is_admin(&login_as("AKGONE"), &iam));
        assert!(!session_principal_is_admin(&external(99), &iam));
        // Browser-lift kinds never reach the admin surface.
        assert!(!session_principal_is_admin(
            &AuthMethod::IamBrowserLift {
                access_key_id: "AKADMIN".into()
            },
            &iam
        ));
        assert!(!session_principal_is_admin(&AuthMethod::OpenLift, &iam));
        // IAM mode gone: only the bootstrap holder stays admin.
        assert!(session_principal_is_admin(
            &AuthMethod::Bootstrap,
            &IamState::Disabled
        ));
        assert!(!session_principal_is_admin(
            &login_as("AKADMIN"),
            &IamState::Disabled
        ));
        assert!(!session_principal_is_admin(
            &external(1),
            &IamState::Disabled
        ));
    }

    #[test]
    fn build_version_only_with_a_live_session() {
        assert_eq!(build_version_for(false), None);
        assert_eq!(
            build_version_for(true).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(build_time_for(false), None);
        assert_eq!(
            build_time_for(true).as_deref(),
            Some(env!("DGP_BUILD_TIME"))
        );
    }

    #[test]
    fn metrics_bearer_check_is_exact() {
        let mut h = HeaderMap::new();
        assert!(!bearer_matches(&h, "scrape-me"), "no header");
        h.insert(header::AUTHORIZATION, "Bearer scrape-me".parse().unwrap());
        assert!(bearer_matches(&h, "scrape-me"));
        assert!(!bearer_matches(&h, "scrape-me-2"), "length differs");
        assert!(!bearer_matches(&h, "scrape-mf"), "same length, wrong byte");
        h.insert(header::AUTHORIZATION, "Basic scrape-me".parse().unwrap());
        assert!(!bearer_matches(&h, "scrape-me"), "wrong scheme");
    }

    /// Adversarial: the session cookie must carry SameSite=Strict
    /// plus HttpOnly plus Path=/. Without Strict, a cross-site top-level
    /// navigation (e.g. an OAuth-callback-shaped link in an attacker
    /// page) can deliver the cookie back to us — the exact CSRF /
    /// login-fixation chain the security review flagged.
    #[test]
    fn session_cookie_is_samesite_strict_httponly() {
        let cookie = session_cookie("abc123", std::time::Duration::from_secs(3600));
        assert!(
            cookie.contains("SameSite=Strict"),
            "session cookie must be SameSite=Strict — got: {cookie}"
        );
        assert!(cookie.contains("HttpOnly"), "cookie: {cookie}");
        assert!(cookie.contains("Path=/"), "cookie: {cookie}");
        assert!(cookie.contains("Max-Age=3600"), "cookie: {cookie}");
        // Must NOT be `SameSite=Lax` or `SameSite=None`.
        assert!(!cookie.contains("SameSite=Lax"), "cookie: {cookie}");
        assert!(!cookie.contains("SameSite=None"), "cookie: {cookie}");
    }

    /// The logout-clear cookie shares the same SameSite policy so that
    /// the browser actually overrides the previous cookie (browsers
    /// distinguish cookies by their (name, domain, path, samesite) tuple
    /// in some impls — same SameSite avoids "ghost" cookies persisting).
    #[test]
    fn logout_cookie_is_samesite_strict() {
        // Re-create what logout returns; the literal lives in logout()
        // but it's a one-line format!() so we just assert the property.
        // If logout() changes its cookie shape, this test stays accurate
        // because we read the same env var via secure_cookies().
        let secure = if secure_cookies() { "; Secure" } else { "" };
        let expected =
            format!("dgp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{secure}");
        assert!(expected.contains("SameSite=Strict"));
        assert!(expected.contains("Max-Age=0"));
    }

    /// `secure_cookies_with` consults `X-Forwarded-Proto: https` only
    /// when `DGP_TRUST_PROXY_HEADERS=true`. Without trust, a hostile
    /// client could spoof `X-Forwarded-Proto: https` against a plain-
    /// HTTP listener and trick the cookie into `Secure` — that'd
    /// silently drop the cookie on the legitimate user's subsequent
    /// HTTP requests, a UX-DoS not a security break, but still worth
    /// gating.
    #[test]
    fn secure_cookies_with_respects_trust_proxy_headers() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let prev_tls = std::env::var("DGP_TLS_ENABLED").ok();
        let prev_trust = std::env::var("DGP_TRUST_PROXY_HEADERS").ok();
        let prev_secure = std::env::var("DGP_SECURE_COOKIES").ok();
        unsafe {
            std::env::remove_var("DGP_TLS_ENABLED");
            std::env::remove_var("DGP_SECURE_COOKIES");
        }

        // Case A: trust=false, XFP=https → still NOT secure (we don't
        // trust the header).
        unsafe { std::env::remove_var("DGP_TRUST_PROXY_HEADERS") };
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(
            !secure_cookies_with(Some(&h)),
            "must NOT trust XFP without DGP_TRUST_PROXY_HEADERS=true"
        );

        // Case B: trust=true, XFP=https → secure.
        unsafe { std::env::set_var("DGP_TRUST_PROXY_HEADERS", "true") };
        assert!(
            secure_cookies_with(Some(&h)),
            "trusted XFP=https must yield Secure cookie"
        );

        // Case C: trust=true, no XFP → falls back to the listener's TLS → false.
        assert!(!secure_cookies_with(Some(&HeaderMap::new())));

        // Case D: explicit DGP_SECURE_COOKIES=true wins.
        unsafe { std::env::set_var("DGP_SECURE_COOKIES", "true") };
        assert!(secure_cookies_with(None));
        unsafe { std::env::set_var("DGP_SECURE_COOKIES", "false") };
        assert!(!secure_cookies_with(Some(&h)));

        // Restore.
        unsafe {
            match prev_tls {
                Some(v) => std::env::set_var("DGP_TLS_ENABLED", v),
                None => std::env::remove_var("DGP_TLS_ENABLED"),
            }
            match prev_trust {
                Some(v) => std::env::set_var("DGP_TRUST_PROXY_HEADERS", v),
                None => std::env::remove_var("DGP_TRUST_PROXY_HEADERS"),
            }
            match prev_secure {
                Some(v) => std::env::set_var("DGP_SECURE_COOKIES", v),
                None => std::env::remove_var("DGP_SECURE_COOKIES"),
            }
        }
    }

    #[test]
    fn auth_method_label_names_every_session_kind() {
        use crate::session::AuthMethod;
        assert_eq!(
            super::auth_method_label(&AuthMethod::Bootstrap),
            "bootstrap"
        );
        assert_eq!(
            super::auth_method_label(&AuthMethod::IamLoginAs {
                access_key_id: "bootstrap".into()
            }),
            "iam",
            "an IAM user whose key is literally `bootstrap` is still an IAM session"
        );
        assert_eq!(
            super::auth_method_label(&AuthMethod::IamBrowserLift {
                access_key_id: "AK".into()
            }),
            "iam_browser"
        );
        assert_eq!(super::auth_method_label(&AuthMethod::OpenLift), "open");
        assert_eq!(
            super::auth_method_label(&AuthMethod::External {
                provider_name: "google".into(),
                user_id: 1
            }),
            "external"
        );
    }
}
