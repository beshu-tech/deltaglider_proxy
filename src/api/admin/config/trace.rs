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

use crate::api::auth::percent_decode;

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

    // Parse path the same way the middleware does: strip leading slash,
    // split on the first '/'.
    let trimmed = body.path.trim_start_matches('/');
    let (bucket_raw, key_raw) = match trimmed.split_once('/') {
        Some((b, k)) => (b.to_string(), k.to_string()),
        None => (trimmed.to_string(), String::new()),
    };
    let bucket = bucket_raw.to_ascii_lowercase();
    let key = if key_raw.is_empty() {
        None
    } else {
        Some(percent_decode(&key_raw))
    };

    // Extract `?prefix=...` from the optional query string, tolerating both
    // `?prefix=x` and `prefix=x`. Missing prefix = empty = bucket-level LIST.
    let query = body.query.as_deref().unwrap_or("");
    let query_trimmed = query.strip_prefix('?').unwrap_or(query);
    let list_prefix = query_trimmed.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == "prefix" {
            Some(percent_decode(v))
        } else {
            None
        }
    });

    // Method: uppercase for match-friendliness. HTTP method names are ASCII
    // and case-insensitive on the wire (`GET` vs `get`); the evaluator
    // compares against canonical uppercase so we normalise here.
    let method = body.method.to_ascii_uppercase();

    let req_info = crate::admission::RequestInfo {
        method: &method,
        bucket: &bucket,
        key: key.as_deref(),
        list_prefix: list_prefix.as_deref(),
        authenticated: body.authenticated,
        source_ip: body.source_ip,
    };

    let decision = crate::admission::evaluate(&chain, &req_info);

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
