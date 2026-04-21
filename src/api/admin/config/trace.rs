//! Admission-trace admin handler (Phase 2).
//!
//! `POST /_/api/admin/config/trace` — dry-run a synthetic request through
//! the admission chain and return the decision the live request path
//! would produce. This is the first slice of the "explain what would
//! happen to this request" tool described in the plan. Later phases
//! layer identity / IAM / parameters / routing decisions on top of the
//! same handler shape.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::AdminState;

/// Synthetic request for the trace endpoint. Mirrors the fields the live
/// admission middleware extracts from `axum::http::Request`, but operator-
/// friendly: path goes in as a single string and we parse `bucket`/`key`
/// the same way the middleware does.
#[derive(Deserialize)]
pub struct TraceRequest {
    /// HTTP method (case-insensitive; we uppercase it before matching).
    pub method: String,
    /// Full request path, e.g. `/my-bucket/releases/v1.zip` or `/my-bucket/`.
    pub path: String,
    /// Optional raw query string (e.g. `prefix=releases/`). Accepted both
    /// with and without a leading `?`.
    #[serde(default)]
    pub query: Option<String>,
    /// Whether the synthetic request carries SigV4 credentials. Matches the
    /// middleware's Authorization-header-or-presigned-query detection.
    #[serde(default)]
    pub authenticated: bool,
    /// Synthetic source IP. When `None`, operator-authored source_ip
    /// predicates evaluate false (fail-closed policy). Accepts any
    /// string parseable as an `IpAddr` (`"203.0.113.5"`, `"2001:db8::1"`).
    #[serde(default)]
    pub source_ip: Option<std::net::IpAddr>,
}

#[derive(Serialize)]
pub struct TraceResponse {
    /// Resolved request inputs the evaluator saw. Echoed back so operators
    /// can verify parsing (e.g. case-folding on the bucket, percent-decoding
    /// on the key).
    pub resolved: TraceResolved,
    /// Admission-layer decision. Phase 2.5+ will add sibling fields for
    /// identity, iam, parameters, and routing.
    pub admission: crate::admission::Decision,
}

#[derive(Serialize)]
pub struct TraceResolved {
    pub method: String,
    pub bucket: String,
    pub key: Option<String>,
    pub list_prefix: Option<String>,
    pub authenticated: bool,
}

/// `POST /_/api/admin/config/trace` — evaluate a synthetic request against
/// the current admission chain.
///
/// The trace handler is deliberately thin: it builds a [`RequestInfo`] from
/// the body using the same normalization rules as the live middleware, then
/// calls [`crate::admission::evaluate`]. The point is that **the same
/// evaluator backs live traffic and trace requests** — operators can trust
/// that a green trace means the real path would produce the same decision.
pub async fn trace_config(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<TraceRequest>,
) -> impl IntoResponse {
    let chain = state.admission_chain.load_full();

    // Route through the same parser the live middleware uses so trace
    // output tracks real traffic exactly. `from_raw` handles path
    // trimming, bucket lowercasing, key percent-decoding, query
    // `?prefix=` extraction, method uppercasing, and IP normalisation.
    let owned = crate::admission::middleware::OwnedRequestInfo::from_raw(
        &body.method,
        &body.path,
        body.query.as_deref().unwrap_or(""),
        body.authenticated,
        body.source_ip,
    );
    let req_info = owned.as_ref();

    let decision = crate::admission::evaluate(&chain, &req_info);
    // `req_info` borrows from `owned` — drop the borrow by scoping
    // it, then move parsed fields out of `owned` for the response.
    let _ = req_info;

    // `TraceResolved` echoes the parsed inputs back so operators can
    // verify the parser agreed with their mental model.
    let OwnedRequestInfoView {
        method,
        bucket,
        key,
        list_prefix,
        ..
    } = OwnedRequestInfoView::from(owned);

    (
        StatusCode::OK,
        Json(TraceResponse {
            resolved: TraceResolved {
                method,
                bucket,
                key,
                list_prefix,
                authenticated: body.authenticated,
            },
            admission: decision,
        }),
    )
}

/// Local view over `OwnedRequestInfo` that collapses the empty-string
/// sentinels (`key: ""`, `list_prefix: ""`) back to `Option::None` so
/// they serialize as `null` in the response body.
struct OwnedRequestInfoView {
    method: String,
    bucket: String,
    key: Option<String>,
    list_prefix: Option<String>,
}

impl From<crate::admission::middleware::OwnedRequestInfo> for OwnedRequestInfoView {
    fn from(o: crate::admission::middleware::OwnedRequestInfo) -> Self {
        Self {
            method: o.method,
            bucket: o.bucket,
            key: if o.key.is_empty() { None } else { Some(o.key) },
            list_prefix: if o.list_prefix.is_empty() {
                None
            } else {
                Some(o.list_prefix)
            },
        }
    }
}
