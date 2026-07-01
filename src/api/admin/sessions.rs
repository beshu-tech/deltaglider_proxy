// SPDX-License-Identifier: GPL-3.0-only

//! Admin session management: list live sessions and force-logout (revoke).
//!
//! Closes the security hole where a stolen admin cookie could only be killed by
//! restarting the whole proxy — rotating the IAM key does NOT invalidate an
//! already-minted session cookie. All routes are admin-GUI-gated.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::{audit_log, AdminState};

/// GET /api/admin/sessions — list live (non-expired) sessions, redacted.
pub async fn list_sessions(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let sessions = state.sessions.list();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "sessions": sessions })),
    )
}

/// DELETE /api/admin/sessions/:id — force-logout one session by its short id.
/// Refuses to revoke the caller's OWN session (use logout for that) so an admin
/// can't accidentally lock themselves out mid-cleanup.
pub async fn revoke_session(
    State(state): State<Arc<AdminState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(own) = super::auth::extract_session_token(&headers) {
        if state.sessions.session_id_matches(&own, &id) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "use logout to end your own session" })),
            );
        }
    }
    let revoked = state.sessions.revoke_by_id(&id);
    if !revoked {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such session" })),
        );
    }
    audit_log("session_revoke", "admin", &id, &headers);
    (StatusCode::OK, Json(serde_json::json!({ "revoked": true })))
}

#[derive(Deserialize)]
pub struct RevokeUserRequest {
    /// Revocation identity: an IAM access_key_id or `provider:user_id` for
    /// external logins (what the sessions list shows in its Identity column).
    pub identity: Option<String>,
    /// Legacy alias for `identity` (older UI builds post this field).
    pub access_key_id: Option<String>,
}

/// Result of a fan-out revocation, so callers can report honestly: how many
/// sessions died HERE, whether the epoch is durable, whether peers were pushed.
pub(crate) struct RevokeOutcome {
    pub revoked_local: usize,
    pub persisted: bool,
    pub pushed: bool,
}

/// Revoke every live session for each identity: locally, durably, and on peers.
/// Ordering IS the security invariant: local kill + in-memory epoch FIRST (this
/// node fails closed immediately), durable epoch second, peer push last.
pub(crate) async fn revoke_identities_everywhere(
    state: &Arc<AdminState>,
    identities: &[String],
) -> RevokeOutcome {
    let now = crate::event_outbox::current_unix_seconds();
    let mut revoked_local = 0;
    for identity in identities {
        revoked_local += state.sessions.revoke_by_identity(identity);
        state.sessions.note_revocation(identity, now);
    }

    let mut persisted = false;
    if let Some(db) = state.config_db.as_ref() {
        let db = db.lock().await;
        persisted = true;
        for identity in identities {
            if let Err(e) = db.revoke_identity_sessions(identity, now) {
                tracing::warn!("revoke: failed to persist revocation for {identity}: {e}");
                persisted = false;
            }
        }
    }

    let mut pushed = false;
    if persisted && !state.config_db_mismatch {
        if let Some(sync) = state.config_sync.as_ref() {
            match sync.upload().await {
                Ok(()) => pushed = true,
                Err(e) => tracing::warn!("revoke: config sync push failed: {e}"),
            }
        }
    }
    RevokeOutcome {
        revoked_local,
        persisted,
        pushed,
    }
}

/// POST /api/admin/sessions/revoke-user — force-logout EVERY session of an
/// identity (IAM access key or external `provider:user_id`), on THIS instance
/// AND — via the synced revocation epoch — every other. The escape hatch when
/// a key or cookie is compromised.
pub async fn revoke_user_sessions(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(req): Json<RevokeUserRequest>,
) -> impl IntoResponse {
    let Some(identity) = req.identity.or(req.access_key_id) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "identity (or access_key_id) is required" })),
        );
    };

    let outcome = revoke_identities_everywhere(&state, std::slice::from_ref(&identity)).await;
    audit_log("session_revoke_user", "admin", &identity, &headers);

    // Peers POLL the sync bucket every 5 minutes: pushed ⇒ they converge within
    // one poll (~300s); persisted-not-pushed ⇒ our next poll flushes the upload
    // first, so up to two cycles (~600s); not persisted ⇒ no cross-instance
    // guarantee at all (local-only revoke).
    let propagation_bound_secs = if outcome.pushed {
        Some(300)
    } else if outcome.persisted {
        Some(600)
    } else {
        None
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "revoked": outcome.revoked_local,
            "revoked_local": outcome.revoked_local,
            "persisted": outcome.persisted,
            "pushed": outcome.pushed,
            "propagation_bound_secs": propagation_bound_secs,
        })),
    )
}
