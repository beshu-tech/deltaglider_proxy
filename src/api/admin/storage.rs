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

/// Read the inputs of the decision from the running process. The spool is
/// read with `would_refuse`: a poll never evicts cached files.
fn status_now(state: &AdminState) -> CapacityStatus {
    let engine = state.s3_state.engine.load();
    let watch = crate::disk_watch::installed();
    let reports = watch.as_ref().map(|w| w.last_reports()).unwrap_or_default();
    let spool_dir = engine.spool_dir().dir().display().to_string();
    capacity_status(CapacityInputs {
        reports: &reports,
        monitor_installed: watch.is_some(),
        reports_age_secs: watch.as_ref().and_then(|w| w.last_reports_age_secs()),
        spool_refusal: engine.spool_dir().would_refuse(),
        spool_dir: &spool_dir,
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
