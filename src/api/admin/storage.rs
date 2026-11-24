// SPDX-License-Identifier: BUSL-1.1

//! `GET /_/api/admin/storage/status`: the storage-capacity decision.
//!
//! Session-light (any valid session), with two views behind one admin check:
//! an admin gets the full [`CapacityStatus`], every other session the
//! separate [`BrowserView`] (no path, no backend name, no free-space figure).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::AdminState;
use crate::capacity::{capacity_status, CapacityInputs, CapacityStatus};

/// Read the inputs of the decision from the running process.
fn status_now(state: &AdminState) -> CapacityStatus {
    let engine = state.s3_state.engine.load();
    let reports = crate::disk_watch::installed()
        .map(|w| w.last_reports())
        .unwrap_or_default();
    capacity_status(CapacityInputs {
        reports: &reports,
        spool_refused: engine.spool_dir().write_refusal().is_some(),
        spool_threshold: engine.spool_threshold(),
        largest_file: engine.max_object_size(),
    })
}

pub async fn storage_status(
    State(state): State<Arc<AdminState>>,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> Response {
    let status = status_now(&state);
    if super::auth::request_is_admin_gui(&state, &headers, connect_info.as_ref()) {
        Json(status).into_response()
    } else {
        Json(status.browser_view()).into_response()
    }
}
