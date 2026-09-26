// SPDX-License-Identifier: BUSL-1.1

//! Admin diagnostics for the durable object-event outbox.

use super::AdminState;
use crate::api::admin::extract::{AdminJson, AdminQuery};
use crate::event_delivery::known_status;
use crate::event_outbox::{
    current_unix_seconds, EventOutboxListQuery as DbEventOutboxListQuery, EventOutboxRecord,
    EventOutboxSort, EventOutboxSortOrder, EventOutboxStatusCounts,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct EventOutboxQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub status: Option<String>,
    pub sort: Option<String>,
    pub order: Option<String>,
}

/// One endpoint's delivery state, with a readable label for the panel.
#[derive(Debug, Serialize)]
pub struct EndpointDeliveryView {
    pub endpoint_id: String,
    /// Redacted URL or channel; `None` when the endpoint left the config.
    pub label: Option<String>,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub updated_at: i64,
}

/// An outbox row plus its per-endpoint delivery state (empty until a target
/// is attempted; Slack rows filtered out by the notify rules stay empty).
#[derive(Debug, Serialize)]
pub struct EventOutboxRowView {
    #[serde(flatten)]
    pub record: EventOutboxRecord,
    pub deliveries: Vec<EndpointDeliveryView>,
}

#[derive(Debug, Serialize)]
pub struct EventOutboxResponse {
    pub rows: Vec<EventOutboxRowView>,
    pub counts: EventOutboxStatusCounts,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
    pub status: Option<String>,
    pub sort: String,
    pub order: String,
    pub delivery_enabled: bool,
    pub delivery_active: bool,
}

#[derive(Debug, Deserialize)]
pub struct RequeueEventOutboxRequest {
    pub ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct RequeueEventOutboxResponse {
    pub requeued: usize,
}

pub async fn list(
    AdminQuery(q): AdminQuery<EventOutboxQuery>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<EventOutboxResponse>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let status = q.status.map(|s| s.trim().to_ascii_lowercase());
    if let Some(status) = status.as_deref() {
        if !known_status(status) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown outbox status: {status}"),
            ));
        }
    }
    let sort_raw = q
        .sort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("occurred_at");
    let sort = EventOutboxSort::parse(sort_raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown outbox sort field: {sort_raw}"),
        )
    })?;
    let order_raw = q
        .order
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "desc".to_string());
    let order = EventOutboxSortOrder::parse(&order_raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown outbox sort order: {order_raw}"),
        )
    })?;

    let delivery = { state.config.read().await.event_delivery.clone() };
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;

    let counts = db
        .event_outbox_status_counts()
        .map_err(super::db_error_reply)?;
    let page = db
        .event_outbox_list(DbEventOutboxListQuery {
            status: status.as_deref(),
            limit,
            offset,
            sort,
            order,
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let ids: Vec<i64> = page.rows.iter().map(|r| r.id).collect();
    let mut deliveries = db
        .event_deliveries_for_many(&ids)
        .map_err(super::db_error_reply)?;
    let labels = crate::event_delivery::endpoint_labels(&delivery);
    let rows = page
        .rows
        .into_iter()
        .map(|record| {
            let deliveries = deliveries
                .remove(&record.id)
                .unwrap_or_default()
                .into_iter()
                .map(|d| EndpointDeliveryView {
                    label: labels.get(&d.endpoint_id).cloned(),
                    endpoint_id: d.endpoint_id,
                    status: d.status,
                    attempts: d.attempts,
                    last_error: d.last_error,
                    updated_at: d.updated_at,
                })
                .collect();
            EventOutboxRowView { record, deliveries }
        })
        .collect();

    Ok(Json(EventOutboxResponse {
        rows,
        counts,
        total: page.total,
        limit,
        offset,
        status,
        sort: sort_raw.to_string(),
        order: order_raw,
        delivery_enabled: delivery.enabled,
        delivery_active: delivery.is_active(),
    }))
}

pub async fn requeue_one(
    Path(id): Path<i64>,
    State(state): State<Arc<AdminState>>,
) -> Result<Json<RequeueEventOutboxResponse>, (StatusCode, String)> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;

    // Preserve `attempts` as delivery history. Requeue only moves a dead row
    // back to pending and makes it immediately claimable by the dispatcher.
    let requeued = db
        .event_outbox_requeue_failed(id, current_unix_seconds())
        .map_err(super::db_error_reply)?;

    if !requeued {
        return Err((
            StatusCode::CONFLICT,
            "event is not failed or does not exist".to_string(),
        ));
    }

    Ok(Json(RequeueEventOutboxResponse { requeued: 1 }))
}

pub async fn requeue_many(
    State(state): State<Arc<AdminState>>,
    AdminJson(req): AdminJson<RequeueEventOutboxRequest>,
) -> Result<Json<RequeueEventOutboxResponse>, (StatusCode, String)> {
    if req.ids.is_empty() {
        return Ok(Json(RequeueEventOutboxResponse { requeued: 0 }));
    }
    if req.ids.len() > 500 {
        return Err((
            StatusCode::BAD_REQUEST,
            "cannot requeue more than 500 events at once".to_string(),
        ));
    }

    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;

    let requeued = db
        .event_outbox_requeue_failed_many(&req.ids, current_unix_seconds())
        .map_err(super::db_error_reply)?;

    Ok(Json(RequeueEventOutboxResponse { requeued }))
}

/// POST /_/api/admin/event-outbox/purge-failed — drop terminal failed rows the
/// listeners have already passed. REFUSES (409) if any failed row sits above the
/// listener-cursor floor — those rows may still be consumed by event-driven
/// replication (the outbox is a shared stream), and purging them would silently
/// drop object events. Requeue those, or wait for replication to drain past them.
pub async fn purge_failed(
    State(state): State<Arc<AdminState>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PurgeFailedResponse>, (StatusCode, String)> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "config DB not available".to_string(),
            )
        })?
        .lock()
        .await;

    // Unlike the background pruner, the operator floor ignores cursor staleness:
    // ANY existing listener cursor pins it — a stalled consumer's unconsumed rows
    // must never become purgeable just because it stopped ticking for an hour.
    // EXCEPTION: replication RETIRED (globally disabled or zero rules) — its
    // orphaned cursor row would otherwise pin the floor forever; fall back to
    // the staleness-filtered floor so webhook-only deployments can still purge.
    let replication_retired = {
        let cfg = state.config.read().await;
        !cfg.replication.enabled || cfg.replication.rules.is_empty()
    };
    let min_keep_id = if replication_retired {
        db.event_outbox_min_active_listener_cursor(
            crate::event_outbox::current_unix_seconds(),
            crate::event_delivery::LISTENER_CURSOR_STALE_SECS,
        )
        .unwrap_or(None)
        .unwrap_or(i64::MAX)
    } else {
        db.event_outbox_min_listener_cursor()
            .unwrap_or(None)
            .unwrap_or(i64::MAX)
    };

    let above = db
        .event_outbox_failed_above_floor(min_keep_id)
        .map_err(super::db_error_reply)?;
    if above > 0 {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "{above} failed event(s) are above the active replication cursor and \
                 may still be consumed — requeue them or wait for replication to drain, \
                 then purge. Refusing to drop unconsumed events."
            ),
        ));
    }

    let purged = db
        .event_outbox_purge_failed(min_keep_id)
        .map_err(super::db_error_reply)?;

    crate::audit::audit_log("event_outbox_purge_failed", "admin", "", &headers, "", "");
    Ok(Json(PurgeFailedResponse { purged }))
}

#[derive(serde::Serialize)]
pub struct PurgeFailedResponse {
    pub purged: usize,
}
