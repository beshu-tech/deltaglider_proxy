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
    pub access_key_id: String,
}

/// POST /api/admin/sessions/revoke-user — force-logout EVERY session of an IAM
/// user (by access_key_id), on THIS instance AND every other. The escape hatch
/// when a key is compromised.
pub async fn revoke_user_sessions(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(req): Json<RevokeUserRequest>,
) -> impl IntoResponse {
    // 1. Local, immediate: drop this node's live sessions for the key.
    let n = state.sessions.revoke_by_access_key(&req.access_key_id);

    // 2. Cross-instance: record the revoke epoch in the synced DB and note it
    //    locally so `entry_valid` rejects the key everywhere (a stolen cookie
    //    minted on another node is invalidated once the revocation syncs). Push
    //    the sync immediately so peers converge in seconds, not the 5-min poll.
    let now = crate::event_outbox::current_unix_seconds();
    state.sessions.note_revocation(&req.access_key_id, now);
    if let Some(db) = state.config_db.as_ref() {
        let write = {
            let db = db.lock().await;
            db.revoke_identity_sessions(&req.access_key_id, now)
        };
        if let Err(e) = write {
            tracing::warn!("revoke-user: failed to persist revocation: {e}");
        } else {
            super::trigger_config_sync(&state);
        }
    }

    audit_log("session_revoke_user", "admin", &req.access_key_id, &headers);
    (StatusCode::OK, Json(serde_json::json!({ "revoked": n })))
}
