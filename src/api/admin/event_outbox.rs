// SPDX-License-Identifier: GPL-3.0-only

//! Admin diagnostics for the durable object-event outbox.

use super::AdminState;
use crate::event_delivery::known_status;
use crate::event_outbox::{
    current_unix_seconds, EventOutboxListQuery as DbEventOutboxListQuery, EventOutboxRecord,
    EventOutboxSort, EventOutboxSortOrder, EventOutboxStatusCounts,
};
use axum::extract::{Path, Query, State};
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

#[derive(Debug, Serialize)]
pub struct EventOutboxResponse {
    pub rows: Vec<EventOutboxRecord>,
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
    Query(q): Query<EventOutboxQuery>,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let page = db
        .event_outbox_list(DbEventOutboxListQuery {
            status: status.as_deref(),
            limit,
            offset,
            sort,
            order,
        })
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(EventOutboxResponse {
        rows: page.rows,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

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
    Json(req): Json<RequeueEventOutboxRequest>,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(RequeueEventOutboxResponse { requeued }))
}

/// POST /_/api/admin/event-outbox/purge-failed — drop ALL terminal failed rows.
/// The escape hatch for a dead target whose failed rows would otherwise grow
/// the DB unbounded (requeue only re-fails them against a still-dead target).
pub async fn purge_failed(
    State(state): State<Arc<AdminState>>,
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

    let purged = db
        .event_outbox_purge_failed()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(PurgeFailedResponse { purged }))
}

#[derive(serde::Serialize)]
pub struct PurgeFailedResponse {
    pub purged: usize,
}
