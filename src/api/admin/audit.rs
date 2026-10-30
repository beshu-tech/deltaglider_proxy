// SPDX-License-Identifier: BUSL-1.1

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

use crate::api::admin::extract::AdminQuery;
use axum::{http::StatusCode, response::IntoResponse, Json};
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
pub async fn get_audit(AdminQuery(q): AdminQuery<AuditQuery>) -> impl IntoResponse {
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

/// GET /_/api/admin/encryption-witness — the full verdict of the
/// multi-instance encryption witness (issue #116 P3): status, the buckets
/// this instance refuses with both encryptions, and the last sync error.
/// `/_/ready` shows the status only (it is unauthenticated). `null` on a
/// single instance.
pub async fn get_encryption_witness(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<super::AdminState>>,
) -> impl IntoResponse {
    let status = state
        .s3_state
        .encryption_witness
        .as_ref()
        .map(|w| w.status());
    (StatusCode::OK, Json(status))
}

/// Body of `POST /_/api/admin/encryption-witness/publish`.
#[derive(Deserialize)]
pub struct WitnessPublishRequest {
    /// Subjects: bucket names, `backend:<name>`, `route:default`.
    pub storages: Vec<String>,
}

/// POST /_/api/admin/encryption-witness/publish — the operator publishes
/// THIS instance's current encryption of the named storages to the
/// multi-instance encryption witness (issue #116 P3), e.g. after a restart
/// rollout (a ConfigMap, an env key) that no apply announced. A plaintext
/// intent needs `X-DGP-Confirm-Encryption-Downgrade: <subject>`. Audited
/// (`bucket_encryption_witness_publish`). 409 on a single instance.
pub async fn publish_encryption_witness(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<super::AdminState>>,
    headers: axum::http::HeaderMap,
    super::extract::AdminJson(body): super::extract::AdminJson<WitnessPublishRequest>,
) -> Result<impl IntoResponse, super::AdminError> {
    use crate::coordination::encryption_witness::PublishError;
    let Some(witness) = state.s3_state.encryption_witness.clone() else {
        return Err(super::AdminError::conflict(
            "no encryption witness: config_sync_bucket is not set (single instance)",
        ));
    };
    if body.storages.is_empty() {
        return Err(super::AdminError::invalid("name at least one storage"));
    }
    let cfg = state.config.read().await.clone();
    let confirmed = crate::config::confirmed_downgrades(Some(&headers));
    match witness
        .publish_subjects(&cfg, &body.storages, &confirmed, &headers)
        .await
    {
        Ok(plan) => Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "published": plan
                    .iter()
                    .map(|((backend, bucket), intent)| serde_json::json!({
                        "backend": backend,
                        "bucket": bucket,
                        "mode": intent.mode,
                        "key_id": intent.key_id,
                    }))
                    .collect::<Vec<_>>(),
                "status": witness.status().status,
            })),
        )),
        Err(PublishError::Refused(r)) => Err(super::AdminError::invalid(r.to_string())),
        Err(PublishError::Failed(e)) => {
            tracing::warn!("encryption witness publish failed: {e}");
            Err(super::AdminError::unavailable(
                "the coordination bucket did not answer; the server log has the details",
            ))
        }
    }
}
