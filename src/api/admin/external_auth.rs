//! API handlers for external authentication: OAuth flow, provider CRUD, group mapping.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
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

/// GET /api/admin/oauth/authorize/:provider — initiate OAuth flow.
/// Returns 302 redirect to the provider's authorization endpoint.
/// Accepts optional `?next=/path` for post-login deep linking.
pub async fn oauth_authorize(
    State(state): State<Arc<AdminState>>,
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

    // Validate and sanitize the `next` parameter — must be a local path starting with /_/
    // to prevent open redirect attacks.
    let next_url = params.next.and_then(|n| {
        let trimmed = n.trim();
        if trimmed.starts_with("/_/") && !trimmed.contains("://") && !trimmed.contains("\\") {
            Some(trimmed.to_string())
        } else {
            None // Reject anything that's not a safe local path
        }
    });

    // Build redirect URI from the request's Host header
    let redirect_uri = build_callback_uri(&req_headers);

    let client_ip = rate_limiter::extract_client_ip(&req_headers);

    match ext_auth.initiate_auth(&provider_name, &redirect_uri, client_ip, next_url) {
        Ok(auth_req) => Redirect::temporary(&auth_req.redirect_url).into_response(),
        Err(ExternalAuthError::ProviderNotFound(_)) => {
            (StatusCode::NOT_FOUND, "Provider not found").into_response()
        }
        Err(ExternalAuthError::DiscoveryFailed(msg)) => {
            tracing::warn!("OAuth authorize failed for '{}': {}", provider_name, msg);
            // Return a user-friendly error page
            error_page(
                "Provider Not Ready",
                &format!(
                "The authentication provider '{}' is not ready. Please try again in a moment. ({})",
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

    // Rate limit OAuth callbacks to prevent abuse.
    // Use the raw Option for session binding (not unwrap_or) — the session validation
    // must see the same value. If no IP is available, the session won't be IP-bound.
    let client_ip_for_rate_limit = rate_limiter::extract_client_ip(&req_headers)
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let client_ip_for_session = rate_limiter::extract_client_ip(&req_headers);
    if state.rate_limiter.is_limited(&client_ip_for_rate_limit) {
        return error_page(
            "Too Many Requests",
            "Too many authentication attempts. Please wait and try again.",
        )
        .into_response();
    }

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
            state.rate_limiter.record_failure(&client_ip_for_rate_limit);
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

    // Evaluate group mapping rules and reconcile memberships
    let rules = db.load_group_mapping_rules().unwrap_or_default();
    let target_groups = mapping::evaluate_mappings(&rules, &identity, provider_config.id);
    if let Err(e) = db.set_user_group_memberships(user.id, &target_groups) {
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
    state.rate_limiter.record_success(&client_ip_for_rate_limit);

    // Create session — use raw Option<IpAddr> so session validation sees the same value
    let token = state.sessions.create_session(
        client_ip_for_session,
        AuthMethod::External {
            provider_name: pending.provider_name.clone(),
            user_id: user.id,
        },
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

    let cookie = super::auth::session_cookie(&token, state.sessions.ttl());

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
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, redirect_to)
        .header(header::SET_COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap()
        .into_response()
}

// ── Provider CRUD (protected endpoints) ──

/// GET /api/admin/ext-auth/providers — list all providers (secrets masked).
pub async fn list_providers(
    State(state): State<Arc<AdminState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let mut providers = db.load_auth_providers().map_err(|e| {
        tracing::error!("Failed to load auth providers: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let provider = db.create_auth_provider(&body).map_err(|e| {
        tracing::error!("Failed to create auth provider: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    audit_log("create_auth_provider", "", &body.name, &req_headers);

    // Rebuild external auth manager
    drop(db);
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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let updated = db.update_auth_provider(id, &body).map_err(|e| {
        tracing::error!("Failed to update auth provider {}: {}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    audit_log("update_auth_provider", "", &updated.name, &req_headers);

    drop(db);
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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    db.delete_auth_provider(id).map_err(|e| {
        tracing::error!("Failed to delete auth provider {}: {}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    audit_log("delete_auth_provider", "", &id.to_string(), &req_headers);

    drop(db);
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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let rules = db.load_group_mapping_rules().map_err(|e| {
        tracing::error!("Failed to load mapping rules: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
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
    if body.match_type == "email_regex" && regex::Regex::new(&body.match_value).is_err() {
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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let rule = db.update_group_mapping_rule(id, &body).map_err(|e| {
        tracing::error!("Failed to update mapping rule {}: {}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    drop(db);
    trigger_config_sync(&state);

    Ok(Json(rule))
}

/// DELETE /api/admin/ext-auth/mappings/:id — delete a mapping rule.
pub async fn delete_mapping(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    db.delete_group_mapping_rule(id).map_err(|e| {
        tracing::error!("Failed to delete mapping rule {}: {}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    drop(db);
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
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;
    let identities = db.list_external_identities().map_err(|e| {
        tracing::error!("Failed to load external identities: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
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

        let target_groups = mapping::evaluate_mappings(&rules, &identity_info, ext_id.provider_id);
        let current_groups = db.get_user_group_ids(ext_id.user_id).unwrap_or_default();

        if target_groups != current_groups {
            if let Err(e) = db.set_user_group_memberships(ext_id.user_id, &target_groups) {
                tracing::warn!(
                    "Failed to sync memberships for user {}: {}",
                    ext_id.user_id,
                    e
                );
                continue;
            }
            memberships_changed += symmetric_diff_count(&current_groups, &target_groups);
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

/// Build the OAuth callback URI from the request's Host header.
fn build_callback_uri(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");

    let scheme = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };

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
fn error_page(title: &str, message: &str) -> impl IntoResponse {
    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head><title>{title}</title>
<style>
  body {{ font-family: system-ui, -apple-system, sans-serif; background: #080c14; color: #e2e8f0;
         display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }}
  .card {{ background: #111827; border: 1px solid #1f2937; border-radius: 12px; padding: 40px;
           max-width: 420px; text-align: center; }}
  h1 {{ font-size: 20px; margin: 0 0 12px; color: #f87171; }}
  p {{ font-size: 14px; color: #9ca3af; line-height: 1.6; margin: 0 0 24px; }}
  a {{ display: inline-block; padding: 10px 24px; background: #2dd4bf; color: #080c14;
       border-radius: 8px; text-decoration: none; font-weight: 600; font-size: 14px; }}
  a:hover {{ background: #14b8a6; }}
</style>
</head>
<body>
  <div class="card">
    <h1>{title}</h1>
    <p>{message}</p>
    <a href="/_/admin">Back to Admin</a>
  </div>
</body>
</html>"#,
        title = title,
        message = message,
    );
    axum::response::Html(html)
}

fn symmetric_diff_count(a: &[i64], b: &[i64]) -> usize {
    let in_a_not_b = a.iter().filter(|x| !b.contains(x)).count();
    let in_b_not_a = b.iter().filter(|x| !a.contains(x)).count();
    in_a_not_b + in_b_not_a
}
