// SPDX-License-Identifier: GPL-3.0-only

//! API handlers for external authentication: OAuth flow, provider CRUD, group mapping.

use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

use crate::config_db::auth_providers::{
    CreateAuthProviderRequest, CreateMappingRuleRequest, UpdateAuthProviderRequest,
    UpdateMappingRuleRequest,
};
use crate::iam::external_auth::mapping;
use crate::iam::external_auth::types::ExternalAuthError;
use crate::iam::keygen;
use crate::rate_limiter;
use crate::session::AuthMethod;

use super::{audit_log, trigger_config_sync, users::rebuild_iam_index, AdminState};

// ── OAuth Flow (public endpoints) ──

#[derive(Deserialize)]
pub struct OAuthAuthorizeQuery {
    /// Post-login redirect path (e.g. "/_/admin/users"). Validated and stored server-side.
    next: Option<String>,
}

#[derive(Deserialize)]
pub struct OAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Pure validator for the `?next=...` deep-link parameter on the
/// OAuth authorize endpoint.
///
/// Pre-fix this validator only checked `starts_with("/_/")`,
/// `!contains("://")`, `!contains("\\")`. Control bytes (CR, LF,
/// NUL, the rest of 0x00–0x1F, plus space and the high half of
/// 0x7F–0xFF) survived the filter, ended up stored in
/// `PendingAuth.redirect_to`, and crashed the OAuth callback when
/// `Response::builder().header(LOCATION, ...).body(...).unwrap()`
/// rejected them as invalid header bytes (E-P0-1).
///
/// Post-fix every byte must be printable visible ASCII
/// (0x21..=0x7E). Anything else → `None`. Open-redirect protection
/// (`/_/` prefix, no scheme, no backslash) preserved.
pub(crate) fn sanitize_next_param(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("/_/") {
        return None;
    }
    if trimmed.contains("://") || trimmed.contains("\\") {
        return None;
    }
    // Only printable visible ASCII (no space — legitimate paths use
    // `%20`; the URL parser has already percent-decoded any encoded
    // bytes by the time we see them, so a literal space here would
    // be an injection attempt).
    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Name of the OAuth-state binding cookie.
const OAUTH_STATE_COOKIE: &str = "dgp_oauth_state";

/// Build the OAuth state-binding cookie. `SameSite=Lax` is required:
/// the IdP→our-proxy redirect is a cross-site GET, and Strict would
/// drop the cookie. `Path` is scoped to the OAuth endpoints so the
/// cookie isn't sent on every admin API call. Short Max-Age (5 min)
/// bounds the flow window.
fn oauth_state_cookie(state_token: &str, req_headers: &HeaderMap) -> String {
    let secure = if super::auth::secure_cookies_with(Some(req_headers)) {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}={}; HttpOnly; SameSite=Lax; Path=/_/api/admin/oauth; Max-Age=300{}",
        OAUTH_STATE_COOKIE, state_token, secure
    )
}

/// Build a cookie that clears the OAuth state cookie. Returned on
/// the callback so the binding token doesn't linger past the flow.
fn oauth_state_clear_cookie(req_headers: &HeaderMap) -> String {
    let secure = if super::auth::secure_cookies_with(Some(req_headers)) {
        "; Secure"
    } else {
        ""
    };
    format!(
        "{}=; HttpOnly; SameSite=Lax; Path=/_/api/admin/oauth; Max-Age=0{}",
        OAUTH_STATE_COOKIE, secure
    )
}

/// Read the OAuth state-binding cookie from the request headers.
/// Pure: no I/O, just `Cookie:` header parsing.
fn extract_oauth_state_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(|s| s.trim())
        .find_map(|c| c.strip_prefix(&format!("{OAUTH_STATE_COOKIE}=")))
        .map(|v| v.to_string())
}

/// GET /api/admin/oauth/authorize/:provider — initiate OAuth flow.
/// Returns 302 redirect to the provider's authorization endpoint.
/// Accepts optional `?next=/path` for post-login deep linking.
pub async fn oauth_authorize(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    Path(provider_name): Path<String>,
    Query(params): Query<OAuthAuthorizeQuery>,
    req_headers: HeaderMap,
) -> Response {
    tracing::info!("OAuth authorize request for provider '{}'", provider_name);
    let ext_auth = match &state.external_auth {
        Some(ea) => ea,
        None => {
            return (StatusCode::NOT_FOUND, "External auth not configured").into_response();
        }
    };

    let next_url = params.next.and_then(|n| sanitize_next_param(&n));

    // Rate-limit: the first hit triggers `discover().await` which
    // makes an outbound HTTP fetch — pair this with the SSRF guards
    // and a hostile caller can still drive a flood of outbound
    // requests at any pace. Same bucket as the callback (per-IP).
    let guard = match crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "oauth_authorize",
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return error_page(
                "Too Many Requests",
                "Too many authentication attempts. Please wait and try again.",
            )
            .into_response();
        }
    };

    // Build redirect URI from the request's Host header
    let redirect_uri = build_callback_uri(&req_headers);

    let client_ip =
        rate_limiter::extract_client_ip_with_peer(&req_headers, connect_info.map(|ci| ci.0.ip()));

    // If discovery isn't cached yet, run it on-demand before initiating auth.
    // This avoids the "provider not ready" error on first click after startup.
    if let Some(provider) = ext_auth.get_provider(&provider_name) {
        if !provider.is_discovery_cached() {
            tracing::info!("Running on-demand OIDC discovery for '{}'", provider_name);
            if let Err(e) = provider.discover().await {
                tracing::warn!("On-demand discovery failed for '{}': {}", provider_name, e);
            }
        }
    }

    match ext_auth.initiate_auth(&provider_name, &redirect_uri, client_ip, next_url) {
        Ok(auth_req) => {
            // Reset the rate limiter — legitimate initiation succeeded.
            guard.record_success();
            // Bind the OAuth `state` token to THIS browser via a
            // short-lived cookie. On callback we cross-check the
            // query-string state against this cookie value; a hostile
            // page that learns a state token from logs / referrer /
            // an opened-callback-link can no longer drive the
            // victim's browser through the flow because they don't
            // have the cookie.
            //
            // SameSite=Lax is mandatory: the IdP→our-proxy redirect
            // is a cross-site GET, and Strict would drop the cookie.
            // Short max-age (5 min) bounds the flow window.
            let oauth_state_cookie = oauth_state_cookie(&auth_req.state, &req_headers);
            (
                StatusCode::TEMPORARY_REDIRECT,
                [
                    (axum::http::header::LOCATION, auth_req.redirect_url),
                    (axum::http::header::SET_COOKIE, oauth_state_cookie),
                ],
            )
                .into_response()
        }
        Err(ExternalAuthError::ProviderNotFound(_)) => {
            (StatusCode::NOT_FOUND, "Provider not found").into_response()
        }
        Err(ExternalAuthError::DiscoveryFailed(msg)) => {
            tracing::error!("OIDC discovery failed for '{}': {}", provider_name, msg);
            error_page(
                "Provider Unavailable",
                &format!(
                    "Could not reach the authentication provider '{}'. Check the issuer URL and network connectivity. ({})",
                    provider_name, msg
                ),
            )
            .into_response()
        }
        Err(e) => {
            tracing::error!("OAuth authorize error: {}", e);
            error_page("Authentication Error", &e.to_string()).into_response()
        }
    }
}

/// GET /api/admin/oauth/callback — OAuth callback handler.
/// Exchanges the authorization code for user identity, provisions/updates the user,
/// creates a session, and redirects to the admin UI.
pub async fn oauth_callback(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    Query(params): Query<OAuthCallbackQuery>,
    req_headers: HeaderMap,
) -> Response {
    tracing::info!(
        "OAuth callback: code={} state={} error={:?}",
        params
            .code
            .as_deref()
            .map(|c| &c[..c.len().min(10)])
            .unwrap_or("none"),
        params
            .state
            .as_deref()
            .map(|s| &s[..s.len().min(10)])
            .unwrap_or("none"),
        params.error,
    );

    // Rate limit OAuth callbacks to prevent abuse. The guard handles the
    // UNSPECIFIED fallback, lockout log, and progressive delay.
    //
    // Session binding needs the ORIGINAL `Option<IpAddr>` (not the
    // fallback) — a session created with UNSPECIFIED would incorrectly
    // bind to "any" address. Keep a separate `extract_client_ip` call
    // that preserves the `None` signal for session creation.
    let guard = match crate::rate_limiter::RateLimitGuard::enter(
        &state.rate_limiter,
        &req_headers,
        connect_info.as_ref().map(|ci| ci.0.ip()),
        "oauth_callback",
    )
    .await
    {
        Ok(g) => g,
        Err(_) => {
            return error_page(
                "Too Many Requests",
                "Too many authentication attempts. Please wait and try again.",
            )
            .into_response();
        }
    };
    let client_ip_for_session =
        rate_limiter::extract_client_ip_with_peer(&req_headers, connect_info.map(|ci| ci.0.ip()));

    // Check for provider error response
    if let Some(err) = &params.error {
        let desc = params
            .error_description
            .as_deref()
            .unwrap_or("No description");
        tracing::warn!("OAuth callback error: {} — {}", err, desc);
        return error_page("Authentication Failed", &format!("{}: {}", err, desc)).into_response();
    }

    let code = match &params.code {
        Some(c) => c,
        None => {
            return error_page("Authentication Failed", "Missing authorization code")
                .into_response();
        }
    };

    let state_token = match &params.state {
        Some(s) => s,
        None => {
            return error_page("Authentication Failed", "Missing state parameter").into_response();
        }
    };

    // CSRF defence: the `state` in the query MUST match the cookie
    // we dropped at `authorize`. A third party that learned the state
    // (logs / referrer / a poisoned-callback-link) can't drive THIS
    // browser through the flow because they don't have the cookie.
    // Done BEFORE consuming the pending entry so a forged callback
    // can't burn the legitimate user's state token.
    let cookie_state = extract_oauth_state_cookie(&req_headers);
    let state_binding_ok = cookie_state
        .as_deref()
        .map(|c| {
            use subtle::ConstantTimeEq;
            c.as_bytes().ct_eq(state_token.as_bytes()).into()
        })
        .unwrap_or(false);
    if !state_binding_ok {
        tracing::warn!(
            "OAuth callback rejected: state cookie mismatch (cookie_present={})",
            cookie_state.is_some()
        );
        guard.record_failure();
        return (
            [(
                axum::http::header::SET_COOKIE,
                oauth_state_clear_cookie(&req_headers),
            )],
            error_page(
                "Authentication Failed",
                "Authentication state cookie missing or mismatched. Please restart the sign-in flow.",
            ),
        )
            .into_response();
    }

    let ext_auth = match &state.external_auth {
        Some(ea) => ea,
        None => {
            return error_page("Authentication Failed", "External auth not configured")
                .into_response();
        }
    };

    // Validate and consume the pending auth
    let pending = match ext_auth.consume_pending(state_token) {
        Ok(p) => p,
        Err(_) => {
            guard.record_failure();
            return error_page(
                "Authentication Failed",
                "Invalid or expired authentication state. Please try again.",
            )
            .into_response();
        }
    };

    // Get the provider
    let provider = match ext_auth.get_provider(&pending.provider_name) {
        Some(p) => p,
        None => {
            return error_page("Authentication Failed", "Provider no longer available")
                .into_response();
        }
    };

    // Exchange code for identity
    let redirect_uri = build_callback_uri(&req_headers);
    let identity = match provider.exchange_code(code, &redirect_uri, &pending).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(
                "OAuth code exchange failed for '{}': {}",
                pending.provider_name,
                e
            );
            return error_page(
                "Authentication Failed",
                &format!("Code exchange failed: {}", e),
            )
            .into_response();
        }
    };

    // Check email_verified if required
    let extra = &provider.extra_config;
    let require_verified = extra
        .get("require_email_verified")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if require_verified && !identity.email_verified {
        return error_page(
            "Authentication Failed",
            "Your email has not been verified by the authentication provider.",
        )
        .into_response();
    }

    // Get the config DB
    let config_db = match &state.config_db {
        Some(db) => db,
        None => {
            return error_page("Authentication Failed", "Config database not available")
                .into_response();
        }
    };

    let db = config_db.lock().await;

    // Look up the provider in config DB to get its ID
    let provider_config = match db.get_auth_provider_by_name(&pending.provider_name) {
        Ok(Some(p)) => p,
        _ => {
            return error_page("Authentication Failed", "Provider not found in database")
                .into_response();
        }
    };

    // Find or create the local user
    let (user, _is_new) = match db.find_external_identity(provider_config.id, &identity.subject) {
        Ok(Some(ext_id)) => {
            // Returning user — update their external identity
            let _ = db.update_external_identity(
                ext_id.id,
                identity.email.as_deref(),
                identity.name.as_deref(),
                Some(&identity.raw_claims),
            );
            match db.get_user_by_id(ext_id.user_id) {
                Ok(user) => (user, false),
                Err(e) => {
                    tracing::error!("Failed to load user {}: {}", ext_id.user_id, e);
                    return error_page("Authentication Failed", "User record not found")
                        .into_response();
                }
            }
        }
        Ok(None) => {
            // First login — auto-provision local IAM user
            let display_name = identity
                .name
                .as_deref()
                .or(identity.email.as_deref())
                .unwrap_or("external-user");

            let ak = keygen::generate_access_key_id();
            let sk = keygen::generate_secret_access_key();

            match db.create_external_user(display_name, &ak, &sk) {
                Ok(user) => {
                    // Create the external identity link
                    if let Err(e) = db.create_external_identity(
                        user.id,
                        provider_config.id,
                        &identity.subject,
                        identity.email.as_deref(),
                        identity.name.as_deref(),
                        Some(&identity.raw_claims),
                    ) {
                        tracing::error!("Failed to create external identity: {}", e);
                    }
                    tracing::info!(
                        "Auto-provisioned external user '{}' (id={}) via '{}'",
                        display_name,
                        user.id,
                        pending.provider_name
                    );
                    (user, true)
                }
                Err(e) => {
                    tracing::error!("Failed to create external user: {}", e);
                    return error_page("Authentication Failed", "Failed to create user account")
                        .into_response();
                }
            }
        }
        Err(e) => {
            tracing::error!("External identity lookup failed: {}", e);
            return error_page("Authentication Failed", "Database error").into_response();
        }
    };

    // Check if user is enabled
    if !user.enabled {
        return error_page(
            "Account Disabled",
            "Your account has been disabled by an administrator.",
        )
        .into_response();
    }

    // Evaluate group mapping rules and MERGE with existing memberships.
    // Manual group assignments (e.g., admin added user to "administrators" via GUI)
    // are preserved — mapping rules only ADD groups, never remove manually-assigned ones.
    let rules = db.load_group_mapping_rules().unwrap_or_default();
    let rule_groups = mapping::evaluate_mappings(&rules, &identity, provider_config.id);
    let existing_groups = db.get_user_group_ids(user.id).unwrap_or_default();
    let merged = merge_group_memberships(existing_groups, &rule_groups);
    if let Err(e) = db.set_user_group_memberships(user.id, &merged) {
        tracing::warn!(
            "Failed to update group memberships for user {}: {}",
            user.id,
            e
        );
    }

    // Rebuild IAM index to reflect the new/updated user and group memberships
    let _ = rebuild_iam_index(&db, &state.iam_state);

    // Trigger config DB sync
    drop(db); // Release lock before triggering sync
    trigger_config_sync(&state);

    // Successful OAuth login — reset rate limiter for this IP
    guard.record_success();

    // Rotate the session: drop any pre-login cookie so an XSS-leaked
    // earlier token can't outlive the OAuth flow.
    super::auth::drop_prior_session(&state, &req_headers);

    // Create session — use raw Option<IpAddr> so session validation sees the same value
    let token = state.sessions.create_session(
        client_ip_for_session,
        AuthMethod::External {
            provider_name: pending.provider_name.clone(),
            user_id: user.id,
        },
        crate::session::SessionKind::AdminGui,
    );

    // Auto-populate S3 credentials
    super::auth::auto_populate_s3_creds(
        &state,
        &token,
        user.access_key_id.clone(),
        user.secret_access_key.clone(),
    )
    .await;

    audit_log(
        "external_login",
        &user.name,
        &pending.provider_name,
        &req_headers,
    );

    let cookie =
        super::auth::session_cookie_with_headers(&token, state.sessions.ttl(), Some(&req_headers));
    let clear_oauth_state = oauth_state_clear_cookie(&req_headers);

    // Determine redirect target:
    // 1. Use the `next` param from the original authorize request (stored in PendingAuth)
    // 2. If `next` points to admin and user isn't admin, fall back to browse
    // 3. Default to browse
    let is_admin = user.is_admin();
    let redirect_to = pending
        .redirect_to
        .as_deref()
        .map(|next| {
            // If the requested path requires admin privileges, check if user has them
            if next.starts_with("/_/admin") && !is_admin {
                "/_/browse" // Fall back — user can't access admin
            } else {
                next
            }
        })
        .unwrap_or("/_/browse");

    tracing::info!(
        "OAuth login successful for '{}' (admin={}) — redirecting to {}",
        user.name,
        is_admin,
        redirect_to
    );

    // Build the response manually to ensure Set-Cookie is included with the redirect.
    //
    // Defence-in-depth: the `next` validator at top of the authorize
    // handler already filters control bytes out of `redirect_to`, so
    // `header(LOCATION, redirect_to)` should never reject the value.
    // But if a future refactor weakens that validator, `.unwrap()`
    // here would panic on attacker-controlled input (E-P0-1). Map a
    // builder failure to "redirect to /_/browse with the cookie" so
    // the request can never panic the task.
    match Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, redirect_to)
        .header(header::SET_COOKIE, &cookie)
        .header(header::SET_COOKIE, &clear_oauth_state)
        .body(axum::body::Body::empty())
    {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            tracing::error!(
                "OAuth callback: failed to build redirect response (redirect_to={:?}): {}",
                redirect_to,
                e
            );
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, "/_/browse")
                .header(header::SET_COOKIE, cookie)
                .body(axum::body::Body::empty())
                .map(|r| r.into_response())
                .unwrap_or_else(|_| {
                    (StatusCode::INTERNAL_SERVER_ERROR, "redirect failed").into_response()
                })
        }
    }
}

// ── Provider CRUD (protected endpoints) ──

/// GET /api/admin/ext-auth/providers — list all providers (secrets masked).
pub async fn list_providers(
    State(state): State<Arc<AdminState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let mut providers =
        super::with_config_db(&state, "load auth providers", |db| db.load_auth_providers()).await?;

    // Mask client secrets
    for p in &mut providers {
        if p.client_secret.is_some() {
            p.client_secret = Some("****".to_string());
        }
    }

    Ok(Json(providers))
}

/// POST /api/admin/ext-auth/providers — create a new provider.
pub async fn create_provider(
    State(state): State<Arc<AdminState>>,
    req_headers: HeaderMap,
    Json(body): Json<CreateAuthProviderRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let provider = super::with_config_db(&state, "create auth provider", |db| {
        db.create_auth_provider(&body)
    })
    .await?;

    audit_log("create_auth_provider", "", &body.name, &req_headers);
    rebuild_external_auth(&state).await;
    trigger_config_sync(&state);

    Ok((StatusCode::CREATED, Json(provider)))
}

/// PUT /api/admin/ext-auth/providers/:id — update a provider.
pub async fn update_provider(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    req_headers: HeaderMap,
    Json(body): Json<UpdateAuthProviderRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let updated = super::with_config_db(&state, "update auth provider", |db| {
        db.update_auth_provider(id, &body)
    })
    .await?;

    audit_log("update_auth_provider", "", &updated.name, &req_headers);
    rebuild_external_auth(&state).await;
    trigger_config_sync(&state);

    Ok(Json(updated))
}

/// DELETE /api/admin/ext-auth/providers/:id — delete a provider.
pub async fn delete_provider(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    req_headers: HeaderMap,
) -> Result<impl IntoResponse, StatusCode> {
    super::with_config_db(&state, "delete auth provider", |db| {
        db.delete_auth_provider(id)
    })
    .await?;

    audit_log("delete_auth_provider", "", &id.to_string(), &req_headers);
    rebuild_external_auth(&state).await;
    trigger_config_sync(&state);

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/ext-auth/providers/:id/test — test provider connectivity.
pub async fn test_provider(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let provider_config = db.get_auth_provider(id).map_err(|e| {
        tracing::error!("Failed to load auth provider {}: {}", id, e);
        StatusCode::NOT_FOUND
    })?;
    drop(db);

    // Build a temporary OIDC provider and test it
    use crate::iam::external_auth::oidc::OidcProvider;
    let client_id = provider_config.client_id.ok_or(StatusCode::BAD_REQUEST)?;
    let client_secret = provider_config.client_secret.unwrap_or_default();
    let issuer_url = provider_config.issuer_url.ok_or(StatusCode::BAD_REQUEST)?;

    let oidc = OidcProvider::new(
        provider_config.name,
        client_id,
        client_secret,
        issuer_url,
        provider_config.scopes,
        provider_config
            .extra_config
            .unwrap_or(serde_json::json!({})),
    );

    let result = oidc.test_connection().await.map_err(|e| {
        tracing::error!("Provider test failed: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(result))
}

// ── Group Mapping Rules (protected) ──

/// GET /api/admin/ext-auth/mappings — list all mapping rules.
pub async fn list_mappings(
    State(state): State<Arc<AdminState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let rules = super::with_config_db(&state, "load mapping rules", |db| {
        db.load_group_mapping_rules()
    })
    .await?;
    Ok(Json(rules))
}

/// POST /api/admin/ext-auth/mappings — create a mapping rule.
pub async fn create_mapping(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<CreateMappingRuleRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    // Validate match_type
    let valid_types = [
        "email_exact",
        "email_domain",
        "email_glob",
        "email_regex",
        "claim_value",
    ];
    if !valid_types.contains(&body.match_type.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Validate regex if applicable
    if body.match_type == "email_regex" && regex_lite::Regex::new(&body.match_value).is_err() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let rule = db.create_group_mapping_rule(&body).map_err(|e| {
        tracing::error!("Failed to create mapping rule: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    drop(db);
    trigger_config_sync(&state);

    Ok((StatusCode::CREATED, Json(rule)))
}

/// PUT /api/admin/ext-auth/mappings/:id — update a mapping rule.
pub async fn update_mapping(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMappingRuleRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    // Validate match_type if provided
    if let Some(ref mt) = body.match_type {
        let valid_types = [
            "email_exact",
            "email_domain",
            "email_glob",
            "email_regex",
            "claim_value",
        ];
        if !valid_types.contains(&mt.as_str()) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    // Validate regex if match_type is (or becomes) email_regex
    let is_regex_type = body
        .match_type
        .as_deref()
        .map(|t| t == "email_regex")
        .unwrap_or(false);
    if is_regex_type {
        if let Some(ref val) = body.match_value {
            if regex_lite::Regex::new(val).is_err() {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let rule = super::with_config_db(&state, "update mapping rule", |db| {
        db.update_group_mapping_rule(id, &body)
    })
    .await?;

    trigger_config_sync(&state);

    Ok(Json(rule))
}

/// DELETE /api/admin/ext-auth/mappings/:id — delete a mapping rule.
pub async fn delete_mapping(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    super::with_config_db(&state, "delete mapping rule", |db| {
        db.delete_group_mapping_rule(id)
    })
    .await?;

    trigger_config_sync(&state);

    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/ext-auth/mappings/preview — preview which groups an email would match.
#[derive(Deserialize)]
pub struct PreviewRequest {
    email: String,
}

#[derive(Serialize)]
pub struct PreviewResponse {
    group_ids: Vec<i64>,
    group_names: Vec<String>,
}

pub async fn preview_mapping(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<PreviewRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let rules = db.load_group_mapping_rules().map_err(|e| {
        tracing::error!("Failed to load mapping rules: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let group_ids = mapping::preview_email_mappings(&rules, &body.email);

    // Resolve group names
    let groups = db.load_groups().unwrap_or_default();
    let group_names: Vec<String> = group_ids
        .iter()
        .filter_map(|id| groups.iter().find(|g| g.id == *id).map(|g| g.name.clone()))
        .collect();

    Ok(Json(PreviewResponse {
        group_ids,
        group_names,
    }))
}

// ── External Identities (protected) ──

/// GET /api/admin/ext-auth/identities — list all external identities.
pub async fn list_identities(
    State(state): State<Arc<AdminState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let identities = super::with_config_db(&state, "load external identities", |db| {
        db.list_external_identities()
    })
    .await?;
    Ok(Json(identities))
}

/// POST /api/admin/ext-auth/sync-memberships — re-evaluate all external users' groups.
#[derive(Serialize)]
pub struct SyncResult {
    users_updated: usize,
    memberships_changed: usize,
}

pub async fn sync_memberships(
    State(state): State<Arc<AdminState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let rules = db.load_group_mapping_rules().unwrap_or_default();
    let identities = db.list_external_identities().unwrap_or_default();
    let providers = db.load_auth_providers().unwrap_or_default();

    let mut users_updated = 0;
    let mut memberships_changed = 0;

    for ext_id in &identities {
        // Skip identities whose provider has been deleted
        if !providers.iter().any(|p| p.id == ext_id.provider_id) {
            continue;
        }

        // Build a minimal identity info for mapping evaluation
        let identity_info = crate::iam::external_auth::types::ExternalIdentityInfo {
            subject: ext_id.external_sub.clone(),
            email: ext_id.email.clone(),
            email_verified: true,
            name: ext_id.display_name.clone(),
            groups: vec![],
            raw_claims: ext_id.raw_claims.clone().unwrap_or(serde_json::json!({})),
        };

        let rule_groups = mapping::evaluate_mappings(&rules, &identity_info, ext_id.provider_id);
        let current_groups = db.get_user_group_ids(ext_id.user_id).unwrap_or_default();

        // Merge: add rule-matched groups to existing memberships (preserve manual assignments)
        let merged = merge_group_memberships(current_groups.clone(), &rule_groups);

        if merged != current_groups {
            if let Err(e) = db.set_user_group_memberships(ext_id.user_id, &merged) {
                tracing::warn!(
                    "Failed to sync memberships for user {}: {}",
                    ext_id.user_id,
                    e
                );
                continue;
            }
            memberships_changed += symmetric_diff_count(&current_groups, &merged);
            users_updated += 1;
        }
    }

    if users_updated > 0 {
        let _ = rebuild_iam_index(&db, &state.iam_state);
        drop(db);
        trigger_config_sync(&state);
    }

    Ok(Json(SyncResult {
        users_updated,
        memberships_changed,
    }))
}

// ── Helpers ──

/// Build the OAuth callback URI from request headers.
/// Respects X-Forwarded-Proto/X-Forwarded-Host from reverse proxies.
fn build_callback_uri(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");

    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| {
            if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
                "http"
            } else {
                "https"
            }
        });

    format!("{}://{}/_/api/admin/oauth/callback", scheme, host)
}

/// Rebuild the ExternalAuthManager from current ConfigDb state.
async fn rebuild_external_auth(state: &Arc<AdminState>) {
    if let (Some(ext_auth), Some(config_db)) = (&state.external_auth, &state.config_db) {
        let db = config_db.lock().await;
        let providers = db.load_auth_providers().unwrap_or_default();
        ext_auth.rebuild(&providers);
        drop(db);
        ext_auth.discover_all().await;
    }
}

/// Simple HTML error page for OAuth callback errors.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

fn error_page(title: &str, message: &str) -> impl IntoResponse {
    let title = escape_html(title);
    let message = escape_html(message);
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
  :root {{ --bg:#080c14; --card:#111827; --border:#1f2937; --text:#e2e8f0; --muted:#9ca3af;
          --error:#f87171; --accent:#2dd4bf; --accent-hover:#14b8a6; --accent-text:#080c14; }}
  :root.light {{ --bg:#f5f7fa; --card:#ffffff; --border:#e2e8f0; --text:#1e293b; --muted:#64748b;
                 --error:#e11d48; --accent:#0d9488; --accent-hover:#0f766e; --accent-text:#ffffff; }}
  body {{ font-family: 'Outfit',system-ui,-apple-system,sans-serif; background:var(--bg); color:var(--text);
         display:flex; align-items:center; justify-content:center; min-height:100vh; margin:0; }}
  .card {{ background:var(--card); border:1px solid var(--border); border-radius:12px; padding:40px;
           max-width:420px; text-align:center; }}
  h1 {{ font-size:20px; margin:0 0 12px; color:var(--error); }} h1[role=alert] {{ }}
  p {{ font-size:14px; color:var(--muted); line-height:1.6; margin:0 0 24px; }}
  a {{ display:inline-block; padding:10px 24px; background:var(--accent); color:var(--accent-text);
       border-radius:8px; text-decoration:none; font-weight:600; font-size:14px; outline-offset:3px; }}
  a:hover {{ background:var(--accent-hover); }}
  a:focus-visible {{ outline:2px solid var(--accent); }}
</style>
</head>
<body>
  <div class="card" role="main">
    <h1 role="alert">{title}</h1>
    <p>{message}</p>
    <a href="/_/" aria-label="Return to DeltaGlider Proxy">Back to Home</a>
  </div>
  <script>try{{var t=localStorage.getItem('dg-theme');if(t==='light'||(!t&&matchMedia('(prefers-color-scheme:light)').matches))document.documentElement.classList.add('light')}}catch(e){{}}</script>
</body>
</html>"#,
        title = title,
        message = message,
    );
    axum::response::Html(html)
}

/// Merge rule-evaluated groups into existing memberships, preserving manual assignments.
/// Returns the union of `existing` and `rule_groups` (order-preserving, no duplicates).
fn merge_group_memberships(existing: Vec<i64>, rule_groups: &[i64]) -> Vec<i64> {
    let mut merged = existing;
    for gid in rule_groups {
        if !merged.contains(gid) {
            merged.push(*gid);
        }
    }
    merged
}

fn symmetric_diff_count(a: &[i64], b: &[i64]) -> usize {
    let in_a_not_b = a.iter().filter(|x| !b.contains(x)).count();
    let in_b_not_a = b.iter().filter(|x| !a.contains(x)).count();
    in_a_not_b + in_b_not_a
}

#[cfg(test)]
mod tests {
    use super::{
        extract_oauth_state_cookie, oauth_state_clear_cookie, oauth_state_cookie,
        sanitize_next_param,
    };
    use axum::http::HeaderMap;

    #[test]
    fn accepts_safe_local_paths() {
        assert_eq!(
            sanitize_next_param("/_/admin/configuration"),
            Some("/_/admin/configuration".to_string())
        );
        assert_eq!(
            sanitize_next_param("/_/browse?prefix=foo/bar"),
            Some("/_/browse?prefix=foo/bar".to_string())
        );
        // Trim is applied.
        assert_eq!(
            sanitize_next_param("  /_/admin  "),
            Some("/_/admin".to_string())
        );
    }

    #[test]
    fn rejects_open_redirect_attempts() {
        // External URL.
        assert_eq!(sanitize_next_param("https://evil.example.com/"), None);
        // Protocol-relative URL.
        assert_eq!(sanitize_next_param("//evil.example.com/"), None);
        // Backslash-trick.
        assert_eq!(sanitize_next_param("/_/\\evil.com"), None);
        // Wrong prefix.
        assert_eq!(sanitize_next_param("/admin/x"), None);
        // Empty.
        assert_eq!(sanitize_next_param(""), None);
    }

    /// E-P0-1 regression: control bytes survived the pre-fix
    /// validator and crashed the OAuth callback when later passed
    /// to `Response::builder().header(LOCATION, ...)`. They MUST
    /// be rejected up-front.
    #[test]
    fn rejects_control_bytes() {
        // CR / LF (the classic header-injection attempt).
        assert_eq!(sanitize_next_param("/_/admin\r\nX-Evil: 1"), None);
        // NUL byte.
        assert_eq!(sanitize_next_param("/_/admin\0"), None);
        // SOH (0x01) — the agent's specific repro example.
        assert_eq!(sanitize_next_param("/_/admin\u{0001}x"), None);
        // DEL (0x7F).
        assert_eq!(sanitize_next_param("/_/admin\u{007F}"), None);
        // Non-ASCII unicode (anything ≥ 0x80).
        assert_eq!(sanitize_next_param("/_/админ"), None);
    }

    #[test]
    fn rejects_literal_space_inside() {
        // Legitimate paths use %20; a literal space here is an
        // injection attempt or malformed input.
        assert_eq!(sanitize_next_param("/_/admin path"), None);
    }

    /// Adversarial: the OAuth state-binding cookie must have the right
    /// shape — SameSite=Lax (the IdP→our-proxy redirect IS cross-site
    /// GET, so Strict would drop it), Path scoped to the OAuth
    /// endpoints, HttpOnly, short Max-Age, and `Secure` when the
    /// caller's headers indicate HTTPS via X-Forwarded-Proto.
    #[test]
    fn oauth_state_cookie_shape() {
        let h = HeaderMap::new();
        let c = oauth_state_cookie("STATE_TOKEN", &h);
        assert!(c.contains("dgp_oauth_state=STATE_TOKEN"), "{c}");
        assert!(c.contains("HttpOnly"), "{c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
        assert!(c.contains("Path=/_/api/admin/oauth"), "{c}");
        assert!(c.contains("Max-Age=300"), "{c}");
    }

    /// Adversarial: `extract_oauth_state_cookie` picks the state out
    /// of a multi-cookie header without confusing the prefix.
    #[test]
    fn extract_oauth_state_cookie_picks_right_value() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "foo=bar; dgp_oauth_state=xyz123; dgp_session=zzz"
                .parse()
                .unwrap(),
        );
        assert_eq!(extract_oauth_state_cookie(&h).as_deref(), Some("xyz123"));

        // Missing cookie → None.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "foo=bar; baz=qux".parse().unwrap(),
        );
        assert_eq!(extract_oauth_state_cookie(&h), None);

        // No cookie header at all → None.
        assert_eq!(extract_oauth_state_cookie(&HeaderMap::new()), None);

        // Prefix-confusion regression: `dgp_oauth_state_bait=…`
        // must NOT match `dgp_oauth_state=…`.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            "dgp_oauth_state_bait=ATTACKER".parse().unwrap(),
        );
        assert_eq!(extract_oauth_state_cookie(&h), None);
    }

    /// `oauth_state_clear_cookie` issues a cookie with the same
    /// Path + SameSite + HttpOnly profile and Max-Age=0 so the
    /// browser actually overrides + deletes the binding cookie.
    #[test]
    fn oauth_state_clear_cookie_shape() {
        let h = HeaderMap::new();
        let c = oauth_state_clear_cookie(&h);
        assert!(c.contains("dgp_oauth_state="), "{c}");
        assert!(c.contains("Max-Age=0"), "{c}");
        assert!(c.contains("Path=/_/api/admin/oauth"), "{c}");
        assert!(c.contains("SameSite=Lax"), "{c}");
    }
}
