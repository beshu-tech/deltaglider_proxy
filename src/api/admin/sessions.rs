// SPDX-License-Identifier: BUSL-1.1

//! Admin session management: list live sessions and force-logout (revoke).
//!
//! Closes the security hole where a stolen admin cookie could only be killed by
//! restarting the whole proxy — rotating the IAM key does NOT invalidate an
//! already-minted session cookie. All routes are admin-GUI-gated.

use crate::api::admin::extract::AdminJson;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;

use super::{audit_log, AdminError, AdminState, JsonError};

/// GET /api/admin/sessions — list live (non-expired) sessions, redacted.
pub async fn list_sessions(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let own = super::auth::extract_session_token(&headers);
    let mut sessions = state.sessions.list();
    for s in &mut sessions {
        s.current = own
            .as_deref()
            .is_some_and(|t| state.sessions.session_id_matches(t, &s.id));
    }
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
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    if let Some(own) = super::auth::extract_session_token(&headers) {
        if state.sessions.session_id_matches(&own, &id) {
            return Err(AdminError::invalid("use logout to end your own session"));
        }
    }
    let revoked = state.sessions.revoke_by_id(&id);
    if !revoked {
        return Err(AdminError::not_found("no such session"));
    }
    audit_log("session_revoke", "admin", &id, &headers);
    Ok(Json(serde_json::json!({ "revoked": true })))
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
/// sessions died HERE, whether the epoch is durable, whether peers were pushed,
/// and whether ANY cross-instance path exists (sync configured + not blocked).
pub(crate) struct RevokeOutcome {
    pub revoked_local: usize,
    pub persisted: bool,
    pub pushed: bool,
    pub sync_reachable: bool,
}

/// Revoke every live session for each identity: locally, durably, and on peers.
/// Ordering IS the security invariant: local kill + in-memory epoch FIRST (this
/// node fails closed immediately), durable epoch second, peer push last.
pub(crate) async fn revoke_identities_everywhere(
    state: &Arc<AdminState>,
    identities: &[String],
) -> RevokeOutcome {
    revoke_identities_with(
        &state.sessions,
        state.config_db.as_deref(),
        identities,
        crate::event_outbox::current_unix_seconds(),
        || super::push_config_sync_now(state),
    )
    .await
}

/// [`revoke_identities_everywhere`] with its three steps injected (unit seam).
async fn revoke_identities_with<Fut>(
    sessions: &crate::session::SessionStore,
    config_db: Option<&tokio::sync::Mutex<crate::config_db::ConfigDb>>,
    identities: &[String],
    now: i64,
    push: impl FnOnce() -> Fut,
) -> RevokeOutcome
where
    Fut: std::future::Future<Output = super::SyncPush>,
{
    let mut revoked_local = 0;
    for identity in identities {
        revoked_local += sessions.revoke_by_identity(identity);
        sessions.note_revocation(identity, now);
    }

    let mut persisted = false;
    if let Some(db) = config_db {
        let db = db.lock().await;
        persisted = true;
        for identity in identities {
            if let Err(e) = db.revoke_identity_sessions(identity, now) {
                tracing::warn!("revoke: failed to persist revocation for {identity}: {e}");
                persisted = false;
            }
        }
    }

    let (pushed, sync_reachable) = if persisted {
        match push().await {
            super::SyncPush::Uploaded => (true, true),
            // Failed pushes are queued (mark_needs_upload) — the poll flush
            // still delivers, so a convergence bound exists.
            super::SyncPush::Failed(_) => (false, true),
            // No sync path at all: disabled, or blocked by a mismatch
            // incident (nothing flushes until recovery) — no bound.
            super::SyncPush::Disabled | super::SyncPush::BlockedMismatch => (false, false),
        }
    } else {
        (false, false)
    };
    RevokeOutcome {
        revoked_local,
        persisted,
        pushed,
        sync_reachable,
    }
}

/// POST /api/admin/sessions/revoke-user — force-logout EVERY session of an
/// identity (IAM access key or external `provider:user_id`), on THIS instance
/// AND — via the synced revocation epoch — every other. The escape hatch when
/// a key or cookie is compromised.
pub async fn revoke_user_sessions(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(req): AdminJson<RevokeUserRequest>,
) -> Result<Json<serde_json::Value>, AdminError<JsonError>> {
    let Some(identity) = req.identity.or(req.access_key_id) else {
        return Err(AdminError::invalid(
            "identity (or access_key_id) is required",
        ));
    };

    let outcome = revoke_identities_everywhere(&state, std::slice::from_ref(&identity)).await;
    audit_log("session_revoke_user", "admin", &identity, &headers);

    // Peers POLL the sync bucket every 5 minutes: pushed ⇒ they converge within
    // one poll (~300s); persisted + queued push ⇒ our poll flushes first, so up
    // to two cycles (~600s); no reachable sync (disabled / mismatch-blocked) or
    // not persisted ⇒ NO cross-instance guarantee (local-only revoke).
    let propagation_bound_secs = if outcome.pushed {
        Some(300)
    } else if outcome.persisted && outcome.sync_reachable {
        Some(600)
    } else {
        None
    };
    Ok(Json(serde_json::json!({
        "revoked": outcome.revoked_local,
        "revoked_local": outcome.revoked_local,
        "persisted": outcome.persisted,
        "pushed": outcome.pushed,
        "propagation_bound_secs": propagation_bound_secs,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::admin::SyncPush;
    use crate::config_db::ConfigDb;
    use crate::session::{AuthMethod, SessionKind, SessionStore};
    use std::sync::atomic::{AtomicBool, Ordering};

    const KEY: &str = "AKIAREVOKED";

    fn store_with_victim() -> (SessionStore, String, String) {
        let store = SessionStore::new();
        let victim = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: KEY.into(),
            },
            SessionKind::AdminGui,
        );
        let bystander = store.create_session(None, AuthMethod::Bootstrap, SessionKind::AdminGui);
        assert!(store.validate(&victim, None));
        (store, victim, bystander)
    }

    /// A config DB whose revocation write fails.
    fn broken_db() -> tokio::sync::Mutex<ConfigDb> {
        let db = ConfigDb::in_memory("test").unwrap();
        db.conn
            .execute_batch("DROP TABLE session_revocations")
            .unwrap();
        assert!(db.revoke_identity_sessions(KEY, 1).is_err());
        tokio::sync::Mutex::new(db)
    }

    /// The local kill comes first: a failed DB write does not skip it, and
    /// nothing is pushed for a revocation that is not durable.
    #[tokio::test]
    async fn a_failed_db_write_still_revokes_locally() {
        let (store, victim, bystander) = store_with_victim();
        let db = broken_db();
        let pushed = AtomicBool::new(false);
        // Ahead of the clock, so the epoch check below is deterministic.
        let now = crate::event_outbox::current_unix_seconds() + 60;
        let outcome = revoke_identities_with(&store, Some(&db), &[KEY.into()], now, || async {
            pushed.store(true, Ordering::SeqCst);
            SyncPush::Uploaded
        })
        .await;
        // The in-memory epoch is set too: a session of the identity created
        // up to `now` is refused.
        let late = store.create_session(
            None,
            AuthMethod::IamLoginAs {
                access_key_id: KEY.into(),
            },
            SessionKind::AdminGui,
        );
        assert!(
            !store.validate(&late, None),
            "the in-memory epoch refuses it"
        );
        assert_eq!(outcome.revoked_local, 1);
        assert!(!outcome.persisted);
        assert!(!outcome.pushed);
        assert!(!outcome.sync_reachable);
        assert!(!pushed.load(Ordering::SeqCst), "no push of a lost write");
        assert!(
            !store.validate(&victim, None),
            "this node rejects the cookie"
        );
        assert!(store.admin_gui_auth_method(&victim, None).is_none());
        assert!(store.validate(&bystander, None));
    }

    /// A failed push leaves the local kill and the durable epoch in place.
    #[tokio::test]
    async fn a_failed_push_keeps_the_local_and_durable_revocation() {
        let (store, victim, _) = store_with_victim();
        let db = tokio::sync::Mutex::new(ConfigDb::in_memory("test").unwrap());
        let outcome = revoke_identities_with(&store, Some(&db), &[KEY.into()], 100, || async {
            SyncPush::Failed("bucket down".into())
        })
        .await;
        assert_eq!(outcome.revoked_local, 1);
        assert!(outcome.persisted);
        assert!(!outcome.pushed);
        assert!(outcome.sync_reachable, "the poll flush still delivers");
        assert!(!store.validate(&victim, None));
        assert_eq!(
            db.lock().await.session_revoked_since(KEY).unwrap(),
            Some(100)
        );
    }

    #[tokio::test]
    async fn no_config_db_still_revokes_locally() {
        let (store, victim, _) = store_with_victim();
        let outcome = revoke_identities_with(&store, None, &[KEY.into()], 100, || async {
            unreachable!("nothing to push without a config DB")
        })
        .await;
        assert_eq!(outcome.revoked_local, 1);
        assert!(!outcome.persisted);
        assert!(!store.validate(&victim, None));
    }
}
