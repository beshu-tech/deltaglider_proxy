// SPDX-License-Identifier: GPL-3.0-only

//! Operational-log viewer handlers for the admin GUI.
//!
//! - `GET /_/api/admin/logs?level=&target=&q=&limit=` — recent backlog from the
//!   in-process log ring (`crate::logs`), server-side filtered.
//! - `GET /_/api/admin/logs/stream?level=&target=&q=` — SSE live tail of the
//!   same events as they happen, filtered by the SAME predicate.
//!
//! Captured at an INFO+ floor (see `crate::logs`); the ring is bounded and
//! per-instance — a GUI convenience for incident debugging, not a log store.

use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use futures::stream::{self, Stream};
use serde::Deserialize;

use crate::logs::{self, LogQuery};

const MAX_LIMIT: usize = 2000;
const DEFAULT_LIMIT: usize = 200;

#[derive(Deserialize, Default)]
pub struct LogsQueryParams {
    limit: Option<usize>,
    /// Minimum severity ("error"|"warn"|"info"|"debug"|"trace").
    level: Option<String>,
    /// Substring match on the event target (module path).
    target: Option<String>,
    /// Substring match over message + fields.
    q: Option<String>,
}

impl LogsQueryParams {
    fn to_log_query(&self) -> LogQuery {
        LogQuery {
            level: self.level.as_deref().and_then(logs::level_from_str),
            target: self.target.clone(),
            q: self.q.clone(),
        }
    }
}

/// GET /_/api/admin/logs — recent log entries (newest first), server-side filtered.
pub async fn get_logs(Query(params): Query<LogsQueryParams>) -> impl IntoResponse {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let query = params.to_log_query();
    // Snapshot the whole ring (bounded), filter, then cap at `limit`.
    let backlog = logs::recent_logs(MAX_LIMIT);
    let entries = logs::filter_logs(&backlog, &query, limit);
    (
        StatusCode::OK,
        Json(serde_json::json!({ "entries": entries, "limit": limit })),
    )
}

/// GET /_/api/admin/logs/stream — SSE live tail (filtered).
pub async fn get_logs_stream(
    Query(params): Query<LogsQueryParams>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let query = params.to_log_query();
    let rx = logs::log_broadcast().subscribe();

    let stream = stream::unfold(rx, move |mut rx| {
        let query = query.clone();
        async move {
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        if !logs::log_matches(&entry, &query) {
                            continue; // filtered out — keep waiting
                        }
                        let event = match Event::default().json_data(&entry) {
                            Ok(e) => e.event("log"),
                            Err(e) => Event::default()
                                .event("error")
                                .data(format!("serialise log: {e}")),
                        };
                        return Some((Ok(event), rx));
                    }
                    // Lagged: a slow consumer fell behind and frames were
                    // dropped. Emit a marker and keep going (correct for a tail).
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let event = Event::default()
                            .event("lagged")
                            .data(format!("dropped {n} log line(s) — consumer fell behind"));
                        return Some((Ok(event), rx));
                    }
                    // Sender gone (never, in practice — it's process-global). End.
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new())
}
