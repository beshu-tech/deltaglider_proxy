//! Audit log viewer handler (Wave 11 of the admin UI revamp).
//!
//! GET `/_/api/admin/audit?limit=N` returns the last N entries from
//! the in-memory audit ring (`crate::audit::recent_audit`). Ordering
//! is newest-first so the GUI can render the list without having to
//! re-sort.
//!
//! The ring is supplementary — admin-compliance operators should
//! still read stdout / their log pipeline for authoritative audit
//! state. This endpoint is a GUI convenience for incident
//! debugging, not a substitute for durable log shipping.

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

/// Ceiling on `?limit` — keeps one burst request from serialising
/// the whole ring into a single response. The ring itself is
/// bounded upstream (`DGP_AUDIT_RING_SIZE`, default 500), so this
/// is really a defensive max rather than a new knob.
const MAX_LIMIT: usize = 500;
const DEFAULT_LIMIT: usize = 100;

#[derive(Deserialize)]
pub struct AuditQuery {
    /// How many of the most-recent entries to return. Clamped to
    /// `[1, MAX_LIMIT]`. Defaults to 100 when absent.
    limit: Option<usize>,
}

/// GET /_/api/admin/audit — recent audit entries, newest first.
pub async fn get_audit(Query(q): Query<AuditQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let entries = crate::audit::recent_audit(limit);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "entries": entries,
            "limit": limit,
        })),
    )
}
